//! 字幕生成与导出器 (SRT / ASS / TXT)

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::segment::{ExportMode, Segment, WordTiming};
use crate::utils::time::{seconds_to_ass_time, seconds_to_srt_time, seconds_to_timestamp};
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
        // `wrap == 0`：无样式入口。带样式的三个可折行格式（SBV / 带时间码纯文本）
        // 由 `write_to_file_with_style` 直接调到 inner，不走这里。
        Self::write_in_place_wrapped(segments, path, format, mode, 0)
    }

    fn write_in_place_wrapped(
        segments: &[Segment],
        path: &Path,
        format: &str,
        mode: ExportMode,
        wrap: usize,
    ) -> Result<()> {
        // 工程文件（剪映 / FCPXML / Premiere）同样认「导出模式」：原文 / 仅译文 / 双语，
        // 有译文时压成一行。此前这三个分支只写原文、直接丢弃译文，用户在剪辑软件里
        // 根本看不到已译好的字幕。
        match format.to_lowercase().as_str() {
            "srt" => Self::write_srt_with_mode(segments, path, mode),
            "ass" => Self::write_ass_with_mode(segments, path, mode),
            "vtt" => Self::write_vtt_with_mode(segments, path, mode),
            // ── 播放器/社区常用的小格式（P1-12）──
            "sbv" => Self::write_sbv_inner(segments, path, mode, wrap),
            "lrc" => Self::write_lrc_with_mode(segments, path, mode),
            "txt-ts" | "txtts" => {
                Self::write_txt_with_timecodes_wrapped(segments, path, mode, wrap)
            }
            "txt" => Self::write_txt_with_mode(segments, path, mode),
            // ── 自动章节（YouTube 章节列表）──
            // 章节不是逐句字幕：一行是「时间 + 标题」，没有 segment → 行 的对应关系。
            // 通用导出链路只拿得到 `segments`，所以这里用兜底分组（按默认 300s 切、
            // 标题取每组首句）；要带模型生成的标题，调用方直接走
            // [`write_chapters_file`]（它吃的是 `Chapter` 列表）。
            "chapters" | "youtube-chapters" | "chapter" => {
                let chapters = crate::utils::chapters::plan_chapters(
                    segments,
                    crate::utils::chapters::DEFAULT_TARGET_SECS,
                );
                Self::write_chapters_inner(&chapters, path)
            }
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

    // ─────────────────────────────────────────────────────────────
    // 小格式：SBV / LRC / 带时间码纯文本
    // ─────────────────────────────────────────────────────────────

    /// SBV（SubViewer）写出：`0:00:01.000,0:00:02.000` 一行时间轴，紧跟一行正文。
    ///
    /// 这是 YouTube 早期字幕与一批国产播放器仍在认的格式。它与 SRT 的差别只在
    /// 时间戳写法（`HH:MM:SS.mmm,HH:MM:SS.mmm`，逗号分隔、点号毫秒、**序号可有可无**），
    /// 但两边都不支持多行 cue 之外的转义，因此正文里的换行一律压成空格：SBV 的
    /// 一条字幕就是一行，塞进换行会让下一句被播放器当成另一条字幕的正文。
    /// 无样式入口：不折行（`wrap == 0`）。
    pub fn write_sbv_with_mode<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
    ) -> Result<()> {
        Self::write_sbv_inner(segments, path, mode, 0)
    }

    fn write_sbv_inner<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
        wrap: usize,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 SBV 文件失败")?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for seg in segments {
            let start = seconds_to_sbv_time(seg.start);
            let end = seconds_to_sbv_time(seg.end);
            let raw = super::segment::export_text_for(seg, mode, with_speaker);
            // SBV 一条字幕只有一行：双语/折行产生的换行必须先压成空格，否则播放器
            // 会把第二行当成另一条字幕，整条轨的时间轴跟着错位。
            let text = escape_sbv_text(&wrap_text_capped(&raw, wrap, WRAP_MAX_LINES))
                .replace(['\r', '\n'], " ");
            writeln!(file, "{start},{end}")?;
            writeln!(file, "{}", text.trim())?;
            writeln!(file)?;
        }
        Ok(())
    }

    /// LRC 唱词写出：每行 `[mm:ss.cc]正文`。
    ///
    /// LRC 只带**起点**时间（这是它本身的设计），因此这里只写起点；结束时间由播放器
    /// 按下一行起点补齐——与导入侧（`reader::parse_lrc`）的规则互为逆运算，写出来
    /// 再读回来时中间各句的时间轴不变。
    pub fn write_lrc_with_mode<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 LRC 文件失败")?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for seg in segments {
            let stamp = seconds_to_lrc_time(seg.start);
            // 一行一句：换行一律压成空格。LRC 播放器同样把一行当一条。
            let raw = super::segment::export_text_for(seg, mode, with_speaker);
            // 先 trim **正文**：导入侧（`reader::parse_lrc`）同样会 `trim` 正文，两侧
            // 口径不一致会让「[00:01.00]hi」与「[00:01.00] hi」来回横跳。必须在这一步
            // 先 trim，再交给 [`escape_lrc_text`]——它给行首 `[` 补的那个隔断空格是
            // 转义产物，绝不能在它之后被 trim 掉。
            let text = escape_lrc_text(raw.trim());
            writeln!(file, "[{}]{}", stamp, text)?;
        }
        Ok(())
    }

    /// 带时间码的纯文本写出：`[00:01:23] 正文`，一行一句。
    ///
    /// 与纯 TXT 的分工：TXT 是给人和下游脚本逐行读的，一行就是一句、没有任何时间
    /// 信息；这份则在「保留一行一句」的同时把起点时间带在身上——贴进会议记录、邮件
    /// 或聊天里，读者能直接看出这句话出现在几分几秒，而不用再开一个播放器对。
    /// 因为不带时间轴的 `.txt` 语义不能变（导入侧按字数合成时间轴），这里用独立的
    /// 格式名，而不是给 TXT 加一个开关。
    pub fn write_txt_with_timecodes<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
    ) -> Result<()> {
        // 与 TXT 一致：一行一句，无样式入口不折行。
        Self::write_txt_with_timecodes_wrapped(segments, path, mode, 0)
    }

    fn write_txt_with_timecodes_wrapped<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
        _wrap: usize,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 TXT 文件失败")?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for seg in segments {
            let stamp = seconds_to_timestamp(seg.start);
            let raw = super::segment::export_text_for(seg, mode, with_speaker);
            // 与 TXT 一致：一行一句；折行参数 0 表示不折。
            let flat = raw.replace(['\r', '\n'], " ");
            writeln!(file, "[{}] {}", stamp, flat.trim())?;
        }
        Ok(())
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
            // 空行先折掉：正文里的空行会把这条 cue 从中间切开，导出→导入就丢内容。
            let text = escape_srt_text(&collapse_blank_lines(&wrap_text_capped(
                &raw,
                wrap,
                WRAP_MAX_LINES,
            )));
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
            // SBV / 带时间码的纯文本同样支持按样式折行（LRC 一行一句，不折）。
            "sbv" => Self::write_atomic(path, |target| {
                Self::write_sbv_inner(segments, target, mode, wrap)
            }),
            "txt-ts" | "txtts" => Self::write_atomic(path, |target| {
                Self::write_txt_with_timecodes_wrapped(segments, target, mode, wrap)
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
        let (
            preset_primary,
            preset_outline,
            preset_back,
            preset_border_style,
            preset_outline_w,
            preset_shadow,
        ) = ass_preset_colors(&style.preset_name);
        let default_cfg = SubtitleStyleConfig::default();
        let primary = if style.preset_name != "白字黑影"
            && style.primary_color == default_cfg.primary_color
        {
            preset_primary.to_string()
        } else {
            style.ass_primary_colour()
        };
        let outline = if style.preset_name != "白字黑影"
            && style.outline_color == default_cfg.outline_color
        {
            preset_outline.to_string()
        } else {
            style.ass_outline_colour()
        };
        let outline_w = if style.preset_name != "白字黑影"
            && (style.outline_width - default_cfg.outline_width).abs() < 0.01
        {
            preset_outline_w
        } else {
            style.outline_width
        };
        let border_style =
            if style.preset_name != "白字黑影" && style.bg_style == default_cfg.bg_style {
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
Style: Default,Microsoft YaHei,{size},{primary},&H000000FF,{outline},{back},{bold_flag},0,0,0,100,100,{spacing},0,{border_style},{outline_w},{shadow},{alignment},{margin_lr},{margin_lr},{margin_v},1
"#,
            size = style.font_size,
            spacing = style.letter_spacing,
            margin_v = style.bottom_margin,
            margin_lr = style.margin_lr,
        );
        write!(file, "{}", header)?;

        // 说话人分色：有说话人标签时，为每个说话人生成一条独立 ASS 样式（不同主色），
        // Dialogue 行按说话人引用对应样式。多人访谈/对谈里一眼分清谁在说。
        // 颜色只改 PrimaryColour，其余（描边/阴影/边距）沿用当前预设，保持视觉一致。
        let mut speaker_styles: std::collections::BTreeMap<u32, String> =
            std::collections::BTreeMap::new();
        // 说话人 → 该说话人样式的主色：带逐词标签的行要用它声明「未读」颜色
        //（`\kf` 只负责时间，颜色得在行内声明，见 `ass_karaoke_text`）。
        let mut speaker_colors: std::collections::BTreeMap<u32, &'static str> =
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
            for (idx, (spk, name)) in speaker_styles.iter().enumerate() {
                let color = speaker_ass_color(idx);
                speaker_colors.insert(*spk, color);
                writeln!(
                    file,
                    "Style: {name},Microsoft YaHei,{size},{color},&H000000FF,{outline},{back},{bold_flag},0,0,0,100,100,{spacing},0,{border_style},{outline_w},{shadow},{alignment},{margin_lr},{margin_lr},{margin_v},1",
                    size = style.font_size,
                    spacing = style.letter_spacing,
                    margin_v = style.bottom_margin,
                    margin_lr = style.margin_lr,
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
            // 顺序固定为「先折行、再插逐词标签、最后转义 + 套本格式自己的标记」：
            // 折行产出的 `\n` 与原文里已有的 `\n`（双语两行）在**同一次**转换里
            // 变成 ASS 的硬换行 `\N`；插标签必须发生在折行**之后**——折行会加换行、
            // 会在断点吃掉空格，先插标签再折行会数错字数甚至把标签劈成两半。
            let raw = super::segment::export_text_for(seg, mode, with_speaker);
            let wrapped = wrap_text_capped(&raw, wrap, WRAP_MAX_LINES);
            // 逐词卡拉OK：只在**原文行**插 `{\kfN}`（N = 厘秒）。没有词级数据、
            // 词与文本对不上、开关关闭、或这一条正文里只有译文时，一律原样输出。
            let text = if style.karaoke_word_highlight
                && !seg.words.is_empty()
                && karaoke_line_is_source(seg, mode)
            {
                // 未读色 = 当前样式这一行的主色（默认样式用配置主色，说话人样式用该
                // 说话人的颜色），已读色 = 高亮色；两个颜色都只在这条带标签的行里声明。
                let base_color = match seg.speaker.and_then(|s| speaker_colors.get(&s).copied()) {
                    Some(color) => color,
                    None => primary.as_str(),
                };
                let lead = format!("\\1c{KARAOKE_SUNG_COLOR}&\\2c{base_color}&");
                match ass_karaoke_text(seg.display_text(), &seg.words, &raw, &wrapped, &lead) {
                    Some(annotated) => annotated,
                    None => {
                        tracing::warn!(
                            index = seg.index,
                            "词级时间戳与当前文本对不上，本句导出不做逐词高亮"
                        );
                        ass_plain_text(&wrapped)
                    }
                }
            } else {
                ass_plain_text(&wrapped)
            };
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
            // 与 SRT 同理：空行会被当作 cue 分隔，先把正文里的空行折掉。
            let text = escape_vtt_text(&collapse_blank_lines(&wrap_text_capped(
                &raw,
                wrap,
                WRAP_MAX_LINES,
            )));
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

    /// 章节文本的实际写盘（不含原子包装）。
    ///
    /// 章节不是「段 → 行」的子集，所以这里不遍历 `segments`，只把
    /// [`crate::utils::chapters::to_youtube_chapters`] 的结果落盘。空章节列表写成
    /// 空文件（而不是不创建文件）：调用方明确要了 `.txt`，缺文件比空文件更难解释。
    fn write_chapters_inner(
        chapters: &[crate::utils::chapters::Chapter],
        path: &Path,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建章节文件失败")?;
        let text = crate::utils::chapters::to_youtube_chapters(chapters);
        if !text.is_empty() {
            // 末尾补一个换行：POSIX 文本惯例，用户往后追加内容时也不会和时间戳粘成一行。
            writeln!(file, "{}", text)?;
        }
        Ok(())
    }
}

/// 把「已定稿的章节列表」写成 YouTube 章节文本（`.txt`）。
///
/// # 为什么是独立入口，而不是给 `write_to_file` 加参数
///
/// `write_to_file` 的输入是 `&[Segment]`（逐句字幕），章节的输入是
/// `&[Chapter]`（时间 + 标题）。二者的数据模型对不上：章节可能来自模型、可能被
/// 用户手改过，标题与任何一句字幕都不是一一对应的。硬塞进同一个签名，只能靠
/// 「传空 segments + 全局状态」这种把戏。所以分成两条路：
///
/// - 手上有 `Vec<Chapter>`（模型已给标题 / 用户已改标题）→ 调本函数；
/// - 只有 `segments` → 调 `write_to_file(segments, path, "chapters")`，走兜底分组。
///
/// 原子性与其它格式一致（[`SubtitleWriter::write_atomic`]：先写 `.part`，全部
/// 成功后才 rename 覆盖），失败不会毁掉用户原有的 `.txt`。
pub fn write_chapters_file<P: AsRef<Path>>(
    chapters: &[crate::utils::chapters::Chapter],
    path: P,
) -> Result<()> {
    let path = path.as_ref();
    SubtitleWriter::write_atomic(path, |target| {
        SubtitleWriter::write_chapters_inner(chapters, target)
    })
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
/// - 返回的扩展名默认**也是** [`SubtitleWriter::write_to_file_with_mode`] 的
///   `format` 参数：取值全部来自 `write_in_place` 认的规范名，调用方可以
///   「拿到什么就写什么」，扩展名与内容天然一致。**同扩展名、不同格式名**的那几项
///   （`txt-ts`、`chapters`）必须走 [`canonical_write_format`]。
/// - 认 writer 的拼写别名：`ebu-tt-d`/`ebutt` 与 `ttml` 同源、`netflix-ttal`
///   与 `ttal` 同源。别名只在这里归一，写盘仍用规范扩展名，否则手改过
///   `config.toml` 的用户会拿到「扩展名与内容对不上」的文件。
/// - 大小写与首尾空白不敏感（`"JSON"`、`" json "` 都按 `json` 处理）。
/// - **未知 / 空格式安全回落 `srt`**：与配置抽屉 `fmt_sel` 的兜底
///   （`_ => "srt"`）一致。老配置或被手工改坏的 `config.toml` 不该导出一个
///   writer 根本写不出来的格式（`write_in_place` 对未知格式是 `bail!`，会直接
///   报错）。剪映 / FCPXML / Premiere XML 走剪辑台的专用导出链路（整目录导出
///   或各自的文件对话框），不属于配置项「字幕输出格式」的取值，故同样回落 `srt`。
///
/// 注意这里返回的**第一个值是扩展名**（不是格式名）：`txt-ts` 与 `chapters`
/// 的扩展名都是 `txt`。写盘时该传哪一个见 [`canonical_write_format`]。
pub fn export_spec_for(fmt: &str) -> (&'static str, &'static str) {
    match fmt.trim().to_lowercase().as_str() {
        "ass" => ("ass", "ASS 特效字幕"),
        "vtt" => ("vtt", "VTT 网页字幕"),
        "sbv" => ("sbv", "SBV 播放器字幕"),
        "lrc" => ("lrc", "LRC 唱词"),
        "txt-ts" | "txtts" => ("txt", "带时间码纯文本"),
        "txt" => ("txt", "TXT 纯文本"),
        // 章节的扩展名也是 `txt`，但写盘格式名是 `chapters`（见 [`canonical_write_format`]）。
        "chapters" | "youtube-chapters" | "chapter" => ("txt", "YouTube 章节列表"),
        "json" => ("json", "JSON 结构化字幕"),
        "ttml" | "ebu-tt-d" | "ebutt" => ("ttml", "EBU-TT-D 广播字幕"),
        "ttal" | "netflix-ttal" => ("ttal", "Netflix TTAL 字幕"),
        // 含 "srt"、空串与任何未知值：统一按 SRT 处理
        _ => ("srt", "SRT 字幕"),
    }
}

/// 配置值 / 别名 → writer 认得的**规范格式名**（写盘时该传的那一个）。
///
/// # 为什么不能拿 [`export_spec_for`] 的第一个返回值当格式名
///
/// 那是**扩展名**，不是格式名：`txt-ts`（带时间码纯文本）的扩展名就是 `txt`。
/// 历史库导出曾经直接拿扩展名当写盘格式，于是用户选了「带时间码纯文本」，
/// 导出内容却是纯文本——文件名对、内容错，而且是静默发生的。
///
/// 这两件事从此各归各位：**扩展名**只用于文件名与对话框过滤器，
/// **规范格式名**只用于写盘。`txt` 与 `txt-ts` 这对同扩展名的组合就是判据
/// （`chapters` 同理：扩展名是 `txt`，格式名是 `chapters`）。
pub fn canonical_write_format(fmt: &str) -> &'static str {
    match fmt.trim().to_lowercase().as_str() {
        "ass" => "ass",
        "vtt" => "vtt",
        "sbv" => "sbv",
        "lrc" => "lrc",
        "txt-ts" | "txtts" => "txt-ts",
        "txt" => "txt",
        // 章节：写盘内容由 `Chapter` 列表决定，`write_to_file` 只用 segments 走兜底分组。
        "chapters" | "youtube-chapters" | "chapter" => "chapters",
        "json" => "json",
        "ttml" | "ebu-tt-d" | "ebutt" => "ttml",
        "ttal" | "netflix-ttal" => "ttal",
        _ => "srt",
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

/// 把正文里连续的**空行**折掉。
///
/// SRT / VTT 用空行分隔 cue：正文里只要出现一个空行，导出文件里的这一条就会被
/// 一分为二，后半段没有时间轴，导入时整段丢失（用户看到「导出的字幕丢了内容」）。
/// 折掉空行后「导出 → 导入」不再丢内容；单个换行（双语/折行的正常形态）原样保留。
fn collapse_blank_lines(text: &str) -> String {
    if !text.contains('\n') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
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

/// 卡拉OK「已唱」高亮色（`&HAABBGGRR`）。
///
/// 与主界面逐词预览的「当前词」强调色同源（薄荷绿）：导出后跟着语音变色的就是这一档。
/// 「未读」色不写死，用当前样式这一行的主色（换预设 / 换说话人配色都跟着走）。
const KARAOKE_SUNG_COLOR: &str = "&H0081B910";

/// 保留字符：覆盖码括号（`{` `}`）、转义前缀（`\`）与 Dialogue 行的字段分隔符（`,`）。
///
/// 逐词标签只允许插在**普通文本之间**。紧贴这些字符时，不同实现可能把标签当成
/// 覆盖码的开头、转义序列的一部分、或字段分隔符来处理。碰到这类边界就不插标签
/// （该词的时长并入上一段），绝不为了多一个标签去赌阅读器的解析差异。
const KARAOKE_RESERVED_CHARS: [char; 4] = ['\\', '{', '}', ','];

/// 逐词卡拉OK标签：`{\kfN}`，N 为**厘秒**（ASS 卡拉OK时间单位，1/100 秒）。
///
/// 用 `\kf`（填充式）而不是 `\k`（整块切换）：一个词往往跨越几百毫秒，`\kf` 在词
/// 时长内平滑扫过，「跟着声音逐词点亮」的观感更贴合。做不到 `\kf` 的阅读器只会
/// 忽略这个标签、退回整行静态渲染——不会错位，也不会把文字弄乱。
/// `lead` 只给首个标签块带属性（颜色声明），后续标签保持最短形式。
fn karaoke_tag(cs: u32, lead: &str) -> String {
    format!("{{{lead}\\kf{cs}}}")
}

/// 折行后的正文 → ASS 最终文本（转义覆盖码 + 把换行换成硬换行 `\N`）。
fn ass_plain_text(wrapped: &str) -> String {
    escape_ass_text(wrapped).replace('\n', "\\N")
}

/// 这一条字幕的正文里是否有「原文行」（逐词标签只插在原文行上）。
///
/// - 原文 / 双语：正文一定有原文行（双语时译文在前、原文在最后一行）；
/// - 仅译文：只有**没有**可用译文时正文才回退成原文，这时才谈得上插标签；
///   一旦有译文，正文就是译文本身——拿原文的词去标译文只会标错地方。
fn karaoke_line_is_source(seg: &Segment, mode: ExportMode) -> bool {
    match mode {
        ExportMode::RawOnly | ExportMode::Bilingual => true,
        ExportMode::TranslationOnly => !seg.has_translation(),
    }
}

/// 规划逐词卡拉OK标签：返回 `(词首字节偏移, 厘秒时长)`，偏移相对 `text`。
///
/// 对齐口径与界面逐词预览同源：按词序在 `text` 里**顺序查找** `word.trim()`。
/// - 词文本可能带前导空格（识别器逐 token 产出），所以比对用 trim 后的内容；
/// - 标点恢复 / 润色会往原文里补标点（`今天天气` → `今天，天气。`），因此不能要求
///   「词拼起来逐字节等于文本」——那会让最常见的工程全部退化成没有高亮；
/// - 但顺序查找依然要求每个词按顺序出现：改字、删词、词表来自旧识别结果都会在
///   某一步找不到词 → 返回 `None`，调用方整句降级、不插任何标签。
///
/// 时长取「本词结束 − 本词开始」的厘秒，**词间与句尾的静音不摊给任何词**：
/// 识别器给的是真实发声区间，把静音摊进词里会让高亮提前于声音；跨句衔接的静音
/// 更不能被上一句的最后一个词吞掉，否则一句说完后字幕还会继续「点亮」到下一句。
fn plan_karaoke_tags(words: &[WordTiming], text: &str) -> Option<Vec<(usize, u32)>> {
    if words.is_empty() || text.is_empty() {
        return None;
    }
    let mut plan: Vec<(usize, u32)> = Vec::with_capacity(words.len());
    let mut cursor = 0usize;
    for w in words {
        let needle = w.word.trim();
        // 纯空白词元、时间非有限的词：与界面逐词预览同一口径，跳过（本就没有可用的
        // 时间），也不当成「对不上」的证据。
        if needle.is_empty() || !w.start.is_finite() || !w.end.is_finite() {
            continue;
        }
        let rel = text[cursor..].find(needle)?;
        let start = cursor + rel;
        cursor = start + needle.len();
        // 时间倒挂（end < start）按 0 处理：`\kf0` 合法，绝不会出现负的厘秒值。
        plan.push((start, ((w.end - w.start).max(0.0) * 100.0).round() as u32));
    }
    if plan.is_empty() {
        None
    } else {
        Some(plan)
    }
}

/// `at` 这个插入位置是否安全：前一个字符与后一个字符都不能是保留字符。
fn karaoke_boundary_is_safe(wrapped: &str, at: usize) -> bool {
    let prev = wrapped[..at].chars().next_back();
    let next = wrapped[at..].chars().next();
    let reserved = |c: char| KARAOKE_RESERVED_CHARS.contains(&c);
    !prev.is_some_and(reserved) && !next.is_some_and(reserved)
}

/// 把 `raw` 上的字节偏移映射到折行后 `wrapped` 上的偏移（长度 `raw.len() + 1`）。
///
/// 折行只做两件事：在断点插入 `\n`、把断点处的空格吃掉；除此之外逐字节保留。
/// 这里用双指针走一遍，既验证这个前提、又顺便算出整张偏移表；一旦发现别的差异
/// （万一折行逻辑将来改了）就返回 `None`，调用方放弃插标签——宁可没有高亮，
/// 也不能把标签插到错的位置上。
fn map_folded_offsets(raw: &str, wrapped: &str) -> Option<Vec<usize>> {
    let rb = raw.as_bytes();
    let wb = wrapped.as_bytes();
    let mut map = vec![0usize; rb.len() + 1];
    let (mut i, mut j) = (0usize, 0usize);
    while i < rb.len() {
        map[i] = j;
        if j < wb.len() && rb[i] == wb[j] {
            i += 1;
            j += 1;
        } else if j < wb.len() && wb[j] == b'\n' {
            // 折行插入的换行：只前进 wrapped 一侧
            j += 1;
        } else if rb[i] == b' ' {
            // 折行在断点吃掉的空格：只前进 raw 一侧
            i += 1;
        } else {
            return None;
        }
    }
    if j != wb.len() {
        return None;
    }
    map[rb.len()] = j;
    Some(map)
}

/// 给一条 ASS 正文插逐词卡拉OK标签；`None` = 保持原样输出（不插任何标签）。
///
/// `text` 是原文行本身（`Segment::display_text()`），`raw` / `wrapped` 分别是折行前、
/// 折行后的整条正文。
///
/// `lead` 是首个标签块里要带上的内联属性（卡拉OK颜色声明）。`\kf` 本身不带颜色，
/// 它依赖「未读 = SecondaryColour、已读 = PrimaryColour」这套状态机，而样式表里的
/// SecondaryColour 是没有任何标签时的占位值（红色）。所以在**带标签的这一行**里
/// 就地声明「已读 = 高亮色、未读 = 当前样式主色」：不动样式表，也不影响同文件里
/// 没有标签的行（没有标签就没有状态机，行仍按样式主色渲染）。
fn ass_karaoke_text(
    text: &str,
    words: &[WordTiming],
    raw: &str,
    wrapped: &str,
    lead: &str,
) -> Option<String> {
    if text.is_empty() || words.is_empty() {
        return None;
    }
    // 原文行必须是正文的后缀：原文 / 双语 / 回退原文三种形态都满足（双语时译文在前、
    // 原文在最后一行）。不满足说明正文结构和预期不符，直接放弃，绝不按猜出来的位置切文本。
    let text_at = raw.len().checked_sub(text.len())?;
    if !raw.ends_with(text) {
        return None;
    }
    let plan = plan_karaoke_tags(words, text)?;
    let offsets = map_folded_offsets(raw, wrapped)?;

    // 词首位置 → 折行后的插入位置。贴着保留字符的边界不插标签：该词的时长并入
    // 上一段计时区间（时间不会被吞掉，只是那一个词跟着上一段一起点亮）。
    let mut inserts: Vec<(usize, u32)> = Vec::with_capacity(plan.len());
    for (off, cs) in plan {
        let at = *offsets.get(text_at + off)?;
        if inserts.is_empty() {
            if !karaoke_boundary_is_safe(wrapped, at) {
                continue;
            }
            inserts.push((at, cs));
        } else if karaoke_boundary_is_safe(wrapped, at) {
            inserts.push((at, cs));
        } else if let Some(last) = inserts.last_mut() {
            last.1 = last.1.saturating_add(cs);
        }
    }
    if inserts.is_empty() {
        return None;
    }

    // 分段转义再拼标签：转义是逐字符替换，分段转义与整段转义逐字节等价，
    // 因此标签本身不会被转义，文本也不会被拆坏。
    let mut out = String::with_capacity(wrapped.len() + inserts.len() * 12);
    let mut prev = 0usize;
    let mut first = true;
    for (at, cs) in inserts {
        out.push_str(&escape_ass_text(&wrapped[prev..at]));
        out.push_str(&karaoke_tag(cs, if first { lead } else { "" }));
        first = false;
        prev = at;
    }
    out.push_str(&escape_ass_text(&wrapped[prev..]));
    Some(out.replace('\n', "\\N"))
}

/// 单条字幕最多折成几行（导出侧的折行入口统一走 [wrap_text_capped]）。
///
/// 取 2：一条字幕折成三行以上会盖掉小半个画面、也更容易和台标/进度条打架，
/// 观众反而更难看全。两行是短横屏与竖屏都能稳定容纳的排版；内容真的塞不下
/// 时 [wrap_text_capped] 会继续往下切，这里的上限不影响完整性。
const WRAP_MAX_LINES: usize = 2;
/// 文本折行：单行不超过 `max_chars`，且连折不超过
/// `max_lines` 行（塞不进时才继续切）。
///
/// 为什么存在：一条字幕折成四五行会盖掉小半个画面。用户配「每行 N 字」的
/// 本意是「别让一行太长、别让整段冒到画面正中」，所以行数上限只是**不主动
/// 多切**——内容确实塞不进上限时照旧切满，宁可行数超一点，也绝不丢字、
/// 不做截断，更不把单词从中间劈开。
fn wrap_text_capped(text: &str, max_chars: usize, max_lines: usize) -> String {
    if max_chars == 0 || max_lines == 0 || text.is_empty() {
        return text.to_string();
    }
    text.split('\n')
        .map(|line| wrap_line_capped(line, max_chars, max_lines))
        .collect::<Vec<_>>()
        .join("\n")
}
/// 单行折行：`max_lines` 是「不主动多切」的上限，不是硬截断。
///
/// 为什么要有上限：一条字幕折成四五行会盖掉小半个画面，远不如「两行塞满」
/// 好读；用户真正在意的是「别冒到画面正中」，而不是每行都短。上限只是**不
/// 主动多切**——内容确实塞不进 `max_lines` 时照旧切满所有行，绝不为了凑行数
/// 丢字或把单词劈开（宁可超行数，也不超字数上限）。
fn wrap_line_capped(line: &str, max_chars: usize, max_lines: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max_chars || max_chars == 0 || max_lines == 0 {
        return line.to_string();
    }
    let candidates = candidate_breaks(&chars, max_chars);
    // 行数越少越清爽：从 1 行开始试，第一个「塞得下」的行数就是排版用的行数；
    // 断点在候选里按均衡目标重新分配，行尾标点与单词边界仍然沿用候选的口径。
    let balanced = assign_breaks(&chars, &candidates, max_chars, max_lines);
    render_breaks(&chars, &balanced)
}

/// 所有**合法断点位置**，递增排列。
///
/// `c[0] == 1`（第一行至少要有一个字符），`c[k-1] == chars.len()`（最后一段
/// 收尾），中间每个 `c[i]` 都是「从 `c[i-1]` 起、在 `max_chars` 内能找到的
/// 最靠右断点」。长度即贪心排版所需的行数。
fn candidate_breaks(chars: &[char], max_chars: usize) -> Vec<usize> {
    let n = chars.len();
    let mut out = Vec::with_capacity(n / max_chars + 1);
    let mut start = 0usize;
    while start < n {
        let end = best_wrap_break(chars, start, max_chars);
        debug_assert!(end > start && end <= n);
        out.push(end);
        // 断点之后的空格不进下一行（行首不留空格）。`canonical_start` 只会
        // 前进，且最多推进到 `n`，因此 `end` 严格递增、不会出现空行。
        start = canonical_start(chars, end);
    }
    out
}

/// 从断点位置出发、跳过紧跟的空格后的「下一行起点」。
///
/// 与 [`best_wrap_break`] 的收尾一致：行首不允许出现空格，所以断点后的空格
/// 既不落在下一行行首，也不必再回吐到本行行尾（本行由 [`render_breaks`] 收尾）。
fn canonical_start(chars: &[char], mut idx: usize) -> usize {
    while idx < chars.len() && chars[idx] == ' ' {
        idx += 1;
    }
    idx
}

/// 把候选断点重新分配给 k 行：每行不超过 max_chars，且切完剩下的字
/// 仍能塞进剩余行数（先保证填得下，再谈均衡，否则会留下排不下的孤行）。
/// 任何一步找不到可行断点就原样返回候选，由调用方退回贪心排版，绝不丢内容。
/// 把候选断点重新分配给 `k` 行：每行不超过 `max_chars`，且切完剩下的字
/// 仍能塞进剩余行数（先保证填得下，再谈均衡，否则会留下排不下的孤行）。
/// 任何一步找不到可行断点就原样返回候选，由调用方退回贪心排版，绝不丢内容。
fn distribute_breaks(
    chars: &[char],
    candidates: &[usize],
    max_chars: usize,
    k: usize,
) -> Vec<usize> {
    let n = chars.len();
    let mut out = Vec::with_capacity(k);
    let mut start = 0usize;
    while out.len() < k {
        // 余下的字全部塞得进本行：收尾，不再多切。
        if n - start <= max_chars {
            out.push(n);
            break;
        }
        let remaining = k - out.len();
        let window = start + max_chars;
        let mut chosen = None;
        for &end in candidates.iter().filter(|&&c| c > start) {
            if end > window {
                break;
            }
            if ends_are_feasible(chars, end, max_chars, remaining - 1) {
                chosen = Some(end);
                break;
            }
        }
        let Some(end) = chosen else {
            // 这一行数分配不出来（余量不够）：退回贪心排版，绝不丢内容。
            return candidates.to_vec();
        };
        out.push(end);
        start = end;
    }
    out
}

/// 「从 `from` 到串尾的文本还能塞进 `lines` 行」的粗粒度可行性判定。
///
/// 只做字符数上界（`剩余字符数 <= 行数 × 每行上限`）：这正是选点需要的余量
/// 信息，而且是 O(1)。它故意**不**考虑断点必须落在标点/空格上的约束——那是
/// [`best_wrap_break`] 的职责，这里只回答「还剩多少余量」。
fn ends_are_feasible(chars: &[char], from: usize, max_chars: usize, lines: usize) -> bool {
    chars.len() - from <= max_chars * lines
}
/// 在贪心行数不变的前提下，找「行数不变但各行更长 / 更短」的最均衡排版。
///
/// 逐步增大目标长度：目标变大只会把断点往右推，因此行数单调不增——循环要么
/// 在 `candidates.len()` 步内耗尽（目标已经推到最长行，不能再均衡），要么
/// 提前拿到「行数更少」的排版直接返回。不会死循环。
fn assign_breaks(
    chars: &[char],
    candidates: &[usize],
    max_chars: usize,
    max_lines: usize,
) -> Vec<usize> {
    let limit = max_lines.min(candidates.len());
    for k in 1..=limit {
        let distributed = distribute_breaks(chars, candidates, max_chars, k);
        if distributed.len() <= k {
            return distributed;
        }
    }
    candidates.to_vec()
}

/// 把断点还原成文本：每段之间插一个 `\n`。
///
/// 断点语义：`breaks[i]` 是第 `i` 行的**结束位置**，且已经保证「断点之后的
/// 空格不进下一行」（见 [`canonical_start`]）。这里只做两件收尾：
/// 1. 打印的行内容从上一个断点开始——断点落在空格上时，那个空格既不留在
///    行尾（行尾空格回退）、也不丢（它属于上一行末尾的那一段空白）；
/// 2. 下一行从下一个断点开始。
///
/// 渲染只负责排版，**绝不能增删字符**：回退掉的空格一定落在某一行里。
fn render_breaks(chars: &[char], breaks: &[usize]) -> String {
    let mut out = String::with_capacity(chars.len() + breaks.len());
    let mut start = 0usize;
    for (idx, &brk) in breaks.iter().enumerate() {
        // 行尾空格回退掉再输出（行首空格由断点本身保证不会出现）。
        let mut end = brk;
        while end > start && chars[end - 1] == ' ' {
            end -= 1;
        }
        out.extend(chars[start..end].iter());
        if idx + 1 < breaks.len() {
            out.push('\n');
        }
        // 断点落在空格上时 `end` 会比 `brk` 小一点：多出来的空格留在上一行的
        // 行尾（上面那段回退），下一行从断点之后**跳掉连续空格**再开始，
        // 整串字符既不丢也不重复。
        start = canonical_start(chars, brk);
    }
    out
}

/// 从 `start` 起、单行不超过 `max_chars` 时，最靠右的合法断点位置（即下一行的起点）。
///
/// 断点优先级（都在「尽量让本行更长」的前提下选择）：
/// 1. **标点**：逗号 / 句号 / 顿号 / 分号 / 冒号 / 问叹号 / 破折号 / 省略号 /
///    右括号与右引号（中英文标点都算）。断点落在标点**之后**——标点留在行尾，
///    下一行不会以标点开头；
/// 2. **空格**：英文按词断行，断点处的空格按惯例**被吞掉**（不留在行尾、
///    也不落到行首），因此 **不会把一个单词切成两半**，行首行尾也不会出现空格；
/// 3. 两者都没有（例如一长串无空格无标点的英文单词）才按字符数**硬切**。
fn best_wrap_break(chars: &[char], start: usize, max_chars: usize) -> usize {
    let n = chars.len();
    if n - start <= max_chars {
        return n;
    }
    // `limit` 是本行最多能放下的字符数；由上面的分支可知 `limit < n`，
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
    let mut next = match brk {
        // 断在空格处：空格归入断点本身，下一行从它之后开始（行首不留空格）。
        Some(p) if drop_space => p + 1,
        Some(p) => p,
        // 窗口内既无标点也无空格：只能硬切
        None => limit,
    };
    // 收尾标点不能被挤到下一行行首：把它们并回本行（连排最多 2 个）。
    // 这会让本行最多**超出 `max_chars` 两个字符**——刻意接受的取舍：
    // 硬切（窗口内既无标点也无空格）本就少见，而「下一行以句号/逗号开头」
    // 是肉眼可见的排版缺陷，宁可让本行多收一个标点。
    let mut merged = 0;
    while next < n && merged < 2 && is_wrap_break_after(chars[next]) {
        next += 1;
        merged += 1;
    }
    // 下一行开头若是空白，一并跳过，避免行首出现空格。
    while next < n && chars[next] == ' ' {
        next += 1;
    }
    next
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
        "&H0081B910", // 薄荷绿（与主色一致）
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

// ── 小格式的时间戳 / 转义 helper ──
//
// 这三个函数只做「秒 → 某格式的时间串」与「文本 → 某格式的安全文本」，不依赖
// `SubtitleWriter` 的任何关联状态，因此是模块级自由函数：SBV / LRC / 带时间码
// 纯文本三个写出函数共用，也便于单独测试。

/// SBV 时间戳：`H:MM:SS.mmm`（小时不补零）。
///
/// 小时位补不补零两种写法都有实现接受，这里取 YouTube 自己导出那份的写法。
/// 复用 `seconds_to_srt_time` 的进位与钳位逻辑，只改分隔符与补零，保证
/// 与 SRT 的时间表示完全一致——同一个 `seg.start` 在两种格式里必须是同一时刻。
fn seconds_to_sbv_time(seconds: f64) -> String {
    let text = seconds_to_srt_time(seconds);
    let (clock, millis) = text.split_once(',').expect("SRT 时间必含毫秒段");
    // 只剥**小时位**的前导零（`00:01:05` → `0:01:05`），分与秒两位必须保留：
    // 对整个串做 trim_start_matches 会把 `00:00:01` 剥成 `:00:01`。
    // 至少留一位，避免小时为 0 时剥出空段。
    let (hours, rest) = match clock.split_once(':') {
        Some((hours, rest)) => (hours, rest),
        None => (clock, ""),
    };
    let hours = hours.trim_start_matches('0');
    let hours = if hours.is_empty() { "0" } else { hours };
    format!("{hours}:{rest}.{millis}")
}

/// LRC 时间戳：`mm:ss.cc`（百分秒，小时位超出 99 分钟时进位到分）。
///
/// 只支持两位分钟是 LRC 规范本身的上限（播放器按 `[mm:ss.xx]` 匹配）；超过
/// 99 分钟的素材会进位成三位分钟，仍可被主流播放器解析，但导入侧只按
/// `mm:ss.cc` 读，因此超过 99 分钟的内容**不要**选 LRC——那是格式的锅，
/// 不是本函数的锅，这里如实进位而不是截断。
fn seconds_to_lrc_time(seconds: f64) -> String {
    let total_cs = (seconds.max(0.0) * 100.0).round() as u64;
    let cs = total_cs % 100;
    let total_s = total_cs / 100;
    format!("{:02}:{:02}.{:02}", total_s / 60, total_s % 60, cs)
}

/// SBV 文本转义：`&` `<` `>` → 实体（与 SRT 同一套判据）。
///
/// SBV 的正文也是纯文本，播放器（含 YouTube 的渲染器）对 `<` 的处理与 SRT
/// 一致，因此复用同一份单遍实现。
fn escape_sbv_text(text: &str) -> String {
    escape_markup_entities(text)
}

/// LRC 文本转义：只处理换行（压成空格）与行首的 `[`。
///
/// 行首的 `[` 会被播放器当成时间标签的开头，导致这一行整条不显示；在它前面补
/// 一个空格即可，不需要引入 LRC 规范之外的转义语法。
fn escape_lrc_text(text: &str) -> String {
    let collapsed = text.replace(['\r', '\n'], " ");
    if collapsed.starts_with('[') {
        format!(" {collapsed}")
    } else {
        collapsed
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

    /// 回归：左右安全边距（MarginL/MarginR）跟随配置，头部与说话人样式行都要落值。
    #[test]
    fn ass_margin_lr_written_to_both_style_lines() {
        let mut a = Segment::new(1, 0.0, 1.0, "甲");
        a.speaker = Some(0);
        let mut b = Segment::new(2, 1.0, 2.0, "乙");
        b.speaker = Some(1);
        let segs = vec![a, b];
        let temp = std::env::temp_dir().join("test_v2w_margin_lr.ass");

        // 0 是最容易被「if > 0 才写」这类逻辑吞掉的边界值
        let zero = SubtitleStyleConfig {
            margin_lr: 0,
            bottom_margin: 40,
            ..SubtitleStyleConfig::default()
        };
        SubtitleWriter::write_ass_with_style(&segs, &temp, ExportMode::RawOnly, &zero).unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();

        let head = content
            .lines()
            .find(|l| l.starts_with("Style: Default,"))
            .expect("缺 Default 样式行");
        assert!(
            head.ends_with(",2,0,0,40,1"),
            "Default 样式左右边距未写入 ASS: {head}"
        );
        let speaker_lines: Vec<&str> = content
            .lines()
            .filter(|l| l.starts_with("Style: Speaker"))
            .collect();
        assert_eq!(speaker_lines.len(), 2, "应有两个说话人样式行: {content}");
        for line in speaker_lines {
            assert!(
                line.ends_with(",2,0,0,40,1"),
                "说话人样式左右边距未同步: {line}"
            );
        }

        // 非默认值同样生效（防止写成常量）
        let wide = SubtitleStyleConfig {
            margin_lr: 123,
            bottom_margin: 40,
            ..SubtitleStyleConfig::default()
        };
        SubtitleWriter::write_ass_with_style(&segs, &temp, ExportMode::RawOnly, &wide).unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();
        assert!(
            content.contains(",2,123,123,40,1"),
            "自定义左右边距未写入 ASS: {content}"
        );

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

    /// `txt-ts`（带时间码纯文本）与 `txt` **共用扩展名**，但写盘格式名必须不同。
    ///
    /// 这条就是那个静默 bug 的判据：历史库导出曾拿 `export_spec_for().0`（扩展名）
    /// 当写盘格式名，用户选了「带时间码纯文本」却得到一堆纯文本——文件名对、内容错。
    /// 其余格式恰好「格式名 == 扩展名」，所以那个写法以前没暴露问题。
    #[test]
    fn canonical_write_format_keeps_txt_ts_distinct_from_txt() {
        assert_eq!(canonical_write_format("txt-ts"), "txt-ts");
        assert_eq!(canonical_write_format("txtts"), "txt-ts");
        assert_eq!(canonical_write_format("txt"), "txt");
        // 这两者扩展名相同、格式名不同，正是「不能拿扩展名当格式名」的原因。
        assert_eq!(export_spec_for("txt-ts").0, export_spec_for("txt").0);
        assert_ne!(
            canonical_write_format("txt-ts"),
            canonical_write_format("txt")
        );
        // 其余格式：格式名 == 扩展名。
        for fmt in ["srt", "ass", "vtt", "sbv", "lrc", "json", "ttml", "ttal"] {
            assert_eq!(canonical_write_format(fmt), fmt);
            assert_eq!(
                export_spec_for(fmt).0,
                fmt,
                "{fmt} 的扩展名与格式名恰好相同"
            );
        }
        // 别名、大小写、空白与未知值：与 `export_spec_for` 同一套归一规则。
        assert_eq!(canonical_write_format("EBU-TT-D"), "ttml");
        assert_eq!(canonical_write_format("  SRT  "), "srt");
        assert_eq!(canonical_write_format("不存在的格式"), "srt");
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
        let out = wrap_text_capped(text, 10, usize::MAX);
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
        let out = wrap_text_capped(text, 20, usize::MAX);
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
        let out = wrap_text_capped(text, 10, usize::MAX);
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
        let out_en = wrap_text_capped(en, 10, usize::MAX);
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
        let out = wrap_text_capped(text, 7, usize::MAX);
        for line in out.split('\n') {
            assert!(line.chars().count() <= 7, "行超长: {line:?}");
        }
        assert_eq!(out.replace('\n', ""), text, "硬切不能丢字符: {out:?}");
    }

    /// 已经是多行的原文（双语 / 用户手打换行）按行独立折行，不会被揉成一团。
    #[test]
    fn wrap_preserves_existing_newlines_per_line() {
        let text = "这是一句很长很长的中文原文字幕内容\nHello there this is a rather long translated subtitle line";
        let out = wrap_text_capped(text, 12, usize::MAX);
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
        assert_eq!(wrap_text_capped(long, 0, usize::MAX), long, "0 表示不折行");
        assert_eq!(
            wrap_text_capped(long, 100, usize::MAX),
            long,
            "文本短于阈值应原样返回"
        );
        assert_eq!(
            wrap_text_capped(long, long.chars().count(), usize::MAX),
            long,
            "恰好等长不折行"
        );
        assert_eq!(wrap_text_capped("", 10, usize::MAX), "");
        // 每一行都不超限时也不应插入多余换行
        assert_eq!(
            wrap_text_capped("短句\n另一短句", 10, usize::MAX),
            "短句\n另一短句"
        );
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

    /// 均衡折行：各行长度接近，且行数不超贪心排版的上限。
    ///
    /// 这是产品级的排版诉求——一行 26 字、一行 2 字的字幕，观众读到第二行会
    /// 以为字幕已经结束。这里锁住「没有哪一行明显比别的行长」。
    #[test]
    fn wrap_balances_line_widths_on_long_cjk() {
        let text = "今天天气很好，我们一起去公园散步，然后回家吃饭，最后各自回家睡觉。";
        let out = wrap_text_capped(text, 8, usize::MAX);
        let lengths: Vec<usize> = out.split('\n').map(|l| l.chars().count()).collect();

        // 折行不能比贪心排版切出更多行（每行上限 8 的贪心结果就该是上界）
        assert!(
            lengths.len() <= 5,
            "行数不应超过贪心排版: {lengths:?} / {out:?}"
        );
        for line in out.split('\n') {
            assert!(line.chars().count() <= 10, "行超长: {line:?} / {out:?}");
        }
        // 均衡的判据：最宽与最窄的差不超过一行上限（30 字 5 行时最宽 9、
        // 最窄 2；贪心填满会在末尾留下更差的参差）。
        let widest = lengths.iter().copied().max().unwrap();
        let narrowest = lengths.iter().copied().min().unwrap();
        assert!(
            widest - narrowest <= 7,
            "不允许出现极端参差（最宽与最窄差 <= 一行上限）: {lengths:?} / {out:?}"
        );
        assert_eq!(out.replace('\n', ""), text, "折行不得增删字符: {out:?}");
    }

    /// 均衡折行不许改字：断行只允许吞掉断点处的空格，非空白字符一个不丢。
    ///
    /// 「断点吞空格」是折行的既有惯例（英文单词不能连着换行符，否则播放器会把
    /// `fox` 和 `jumps` 粘成一个词）；本条用「去掉所有空白后逐字相等」锁住它。
    #[test]
    fn wrap_balanced_never_drops_or_duplicates_chars() {
        let cases = [
            (
                "The quick brown fox jumps over the lazy dog near the river bank",
                20usize,
            ),
            ("今天天气很好，我们一起去公园散步，然后回家吃饭。", 10),
            ("aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd", 7),
            ("Mixing 中英文 and punctuation, 一起 break 这行文字", 9),
        ];
        let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        for (text, max_chars) in cases {
            let out = wrap_text_capped(text, max_chars, usize::MAX);
            assert_eq!(
                strip(&out),
                strip(text),
                "折行不得增删非空白字符: {out:?} (max={max_chars})"
            );
            assert!(
                !out.is_empty() && out.split('\n').all(|l| !l.is_empty()),
                "不允许出现空行: {out:?}"
            );
            for line in out.split('\n') {
                assert!(
                    line.chars().count() <= max_chars + 2,
                    "行超长（允许并回标点 +2）: {line:?} / {out:?}"
                );
            }
        }
    }

    /// 行数上限：塞得进两行的长句折成两行，别把画面盖掉大半。
    #[test]
    fn wrap_capped_keeps_long_segments_within_two_lines() {
        // 24 字（汉字算 1）：每行上限 12 时两行就够（12 + 12）。
        let text = "今天天气很好我们一起去看电影然后回家吃饭睡觉吧";
        let out = wrap_text_capped(text, 12, 2);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(
            lines.len(),
            2,
            "24 字在每行 12 字的约束下塞得进两行，不该折三行以上: {out:?}"
        );
        for line in &lines {
            assert!(line.chars().count() <= 14, "行超长: {line:?} / {out:?}");
        }
        assert_eq!(out.replace('\n', ""), text, "上限不得丢字: {out:?}");
    }

    /// 上限只是「不主动多切」：真的塞不进两行时照旧切满，绝不截断。
    #[test]
    fn wrap_capped_never_truncates_when_two_lines_are_not_enough() {
        let text = "abcdefghijklmnopqrstuvwxyz0123456789"; // 36 字
        let out = wrap_text_capped(text, 8, 2);
        let lines: Vec<&str> = out.split('\n').collect();
        assert!(lines.len() > 2, "塞不下时必须继续切而不是截断: {out:?}");
        assert_eq!(out.replace('\n', ""), text, "上限不得丢字: {out:?}");
        for line in &lines {
            assert!(line.chars().count() <= 10, "行超长: {line:?} / {out:?}");
        }
    }

    /// `wrap_text_capped` 的边界：0 上限 / 0 宽度 / 空串都原样返回，不 panic。
    #[test]
    fn wrap_capped_is_noop_for_degenerate_limits() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        assert_eq!(wrap_text_capped(text, 8, 0), text, "行数上限为 0 = 不折");
        assert_eq!(wrap_text_capped(text, 0, 2), text, "宽度为 0 = 不折");
        assert_eq!(wrap_text_capped("", 8, 2), "");
        assert_eq!(wrap_text_capped("短句\n另一短句", 10, 2), "短句\n另一短句");
    }

    /// 上限按**每一行**分别计算：原文里的换行先按段切开，再各自折行，互不挤占。
    ///
    /// 这里是双语字幕的排版：中文一行、英文一行，两行各自成段，各自的折行结果
    /// 都受同一个「每行 N 字」约束，但行数上限不会把两段合起来算。
    #[test]
    fn wrap_capped_applies_per_existing_line() {
        let zh = "今天天气很好，我们一起去公园散步，然后回家吃饭。";
        let en = "The quick brown fox jumps over the lazy dog and keeps running";
        let text = format!("{zh}\n{en}");
        let out = wrap_text_capped(&text, 10, 2);
        let lines: Vec<&str> = out.split('\n').collect();
        // 每段至少折成两行（每行 10 字的约束下中英都放不下），但两段的合计
        // 行数不能超过「各自至少两行」太多——合并计算会把这个数压到 2。
        assert!(lines.len() >= 4, "两段都要各自折行，不能合并计算: {out:?}");
        // 两段的最后一行必须来自各自的内容（英文段尾词在最后）。
        assert!(
            lines.last().is_some_and(|l| l.contains("running")),
            "英文段应完整保留到结尾: {out:?}"
        );
        for line in &lines {
            assert!(line.chars().count() <= 12, "行超长: {line:?} / {out:?}");
        }
    }

    /// 无词级数据（老工程 / 非 Whisper 引擎）时的 ASS 期望值：取自加逐词高亮之前的
    /// 导出器输出，转成字面量后钉死在这里。
    const GOLDEN_ASS: &str = "[Script Info]\nTitle: Voice2Word Subtitle\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\nTimer: 100.0000\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Microsoft YaHei,40,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,1,0,0,0,100,100,1,0,1,2,1,2,20,20,40,1\nStyle: Speaker1,Microsoft YaHei,40,&H0081B910,&H000000FF,&H00000000,&H80000000,1,0,0,0,100,100,1,0,1,2,1,2,20,20,40,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:00:01.50,Speaker1,,0,0,0,,说话人 1: First line\\N第一句\nDialogue: 0,0:00:01.50,0:00:03.00,Default,,0,0,0,,第二句\n";
    ///
    /// 回归：没有词级时间戳时输出必须与加这个功能之前**逐字节一致**——不插标签、
    /// 不声明颜色、不多任何前缀。逐词高亮是「有数据才生效」的增强，不许动老工程。
    #[test]
    fn ass_without_word_timings_is_byte_identical() {
        let mut first = Segment::new(1, 0.0, 1.5, "第一句");
        first.translation = Some("First line".to_string());
        first.speaker = Some(0);
        let second = Segment::new(2, 1.5, 3.0, "第二句");
        let segs = vec![first, second];
        let temp = std::env::temp_dir().join("test_v2w_karaoke_golden.ass");
        SubtitleWriter::write_to_file(&segs, &temp, "ass").unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();
        assert_eq!(
            content, GOLDEN_ASS,
            "无词级数据时 ASS 必须逐字节不变（开关默认开启也不得影响）"
        );
        let _ = std::fs::remove_file(temp);

        // 带样式的入口（不折行）同样逐字节一致：两条导出入口都不许无中生有。
        let styled = write_ass_text(&segs, ExportMode::Bilingual, &karaoke_test_style());
        assert_eq!(
            styled, GOLDEN_ASS,
            "带样式入口在无词级数据时也必须逐字节一致"
        );
    }

    /// 逐词标签回归用：把一批片段按给定样式导出成 ASS 并读回文本。
    ///
    /// 临时文件名带 pid + 序号，避免并发跑测试时几个用例抢同一个文件。
    fn write_ass_text(segs: &[Segment], mode: ExportMode, style: &SubtitleStyleConfig) -> String {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temp = std::env::temp_dir().join(format!(
            "test_v2w_karaoke_{}_{}.ass",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        SubtitleWriter::write_ass_with_style(segs, &temp, mode, style).unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();
        let _ = std::fs::remove_file(&temp);
        content
    }

    /// 逐词标签回归用：取回每条 Dialogue 的正文（第 10 个字段，允许含逗号）。
    fn ass_dialogue_texts(content: &str) -> Vec<String> {
        content
            .lines()
            .filter(|l| l.starts_with("Dialogue:"))
            .map(|l| l.splitn(10, ',').nth(9).unwrap_or("").to_string())
            .collect()
    }

    /// 逐词标签回归用：取出正文里所有 `\kfN` 的 N（按出现顺序）。
    fn karaoke_centiseconds(body: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(pos) = rest.find("\\kf") {
            let tail = &rest[pos + 3..];
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(v) = digits.parse::<u32>() {
                out.push(v);
            }
            rest = &tail[digits.len()..];
        }
        out
    }

    /// 逐词标签回归用：去掉所有 `{...}` 覆盖块，只留字幕文字。
    ///
    /// 用来核对「插标签不改动文字」：标签以外的字符必须与不插标签时逐字节相同。
    fn strip_override_blocks(body: &str) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        for c in body.chars() {
            match c {
                '{' => depth += 1,
                '}' if depth > 0 => depth -= 1,
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    }

    /// 无折行的样式（测试里只看正文，不掺排版差异）。
    fn karaoke_test_style() -> SubtitleStyleConfig {
        SubtitleStyleConfig {
            max_chars_per_line: 0,
            ..SubtitleStyleConfig::default()
        }
    }

    /// 词级时间戳与文本对齐时：每个词一个 `\kf` 标签，厘秒值 = 本词时长，顺序与文本
    /// 一致；首个标签块带上颜色声明（已读 = 高亮色，未读 = 当前样式主色）。
    #[test]
    fn ass_karaoke_tags_follow_word_timings() {
        let mut seg = Segment::new(1, 0.0, 2.0, "今天天气很好");
        seg.words = vec![
            WordTiming {
                word: " 今天".to_string(),
                start: 0.0,
                end: 1.0,
            },
            WordTiming {
                word: "天气".to_string(),
                start: 1.0,
                end: 1.25,
            },
            WordTiming {
                word: " 很好".to_string(),
                start: 1.25,
                end: 2.0,
            },
        ];
        let content = write_ass_text(&[seg], ExportMode::RawOnly, &karaoke_test_style());
        let body = &ass_dialogue_texts(&content)[0];

        assert_eq!(
            karaoke_centiseconds(body),
            vec![100, 25, 75],
            "厘秒值与顺序都要对: {body}"
        );
        assert!(
            body.contains("{\\1c&H0081B910&\\2c&H00FFFFFF&\\kf100}今天{\\kf25}天气{\\kf75}很好"),
            "标签应紧贴各词、首个块声明颜色: {body}"
        );
        assert_eq!(
            body.matches("\\1c").count(),
            1,
            "颜色声明只应出现一次: {body}"
        );
    }

    /// 词级时间戳与当前文本对不上（改过原文 / 词表来自旧识别结果）：整句降级、
    /// 不插任何标签，输出与「本来就没有词级数据」逐字节一致——宁可没有高亮，
    /// 也不能把文字切错。
    #[test]
    fn ass_karaoke_degrades_when_words_do_not_match_text() {
        let mut mismatched = Segment::new(1, 0.0, 2.0, "今天天气很好");
        mismatched.words = vec![
            WordTiming {
                word: " 昨天".to_string(),
                start: 0.0,
                end: 1.0,
            },
            WordTiming {
                word: "不错".to_string(),
                start: 1.0,
                end: 2.0,
            },
        ];
        let plain = Segment::new(1, 0.0, 2.0, "今天天气很好");
        let style = karaoke_test_style();

        let degraded = write_ass_text(&[mismatched], ExportMode::RawOnly, &style);
        let no_words = write_ass_text(&[plain], ExportMode::RawOnly, &style);
        assert!(!degraded.contains("\\kf"), "对不上时不得插标签: {degraded}");
        assert_eq!(degraded, no_words, "降级输出必须与无词级数据完全一致");
    }

    /// 仅译文模式：正文就是译文本身，绝不拿原文的词去标译文行；译文为空回退原文时照插。
    #[test]
    fn ass_karaoke_skips_translation_only_output() {
        let mut seg = Segment::new(1, 0.0, 2.0, "你好世界");
        seg.translation = Some("Hello world".to_string());
        seg.words = vec![
            WordTiming {
                word: "你好".to_string(),
                start: 0.0,
                end: 1.0,
            },
            WordTiming {
                word: "世界".to_string(),
                start: 1.0,
                end: 2.0,
            },
        ];
        let style = karaoke_test_style();

        let content = write_ass_text(
            std::slice::from_ref(&seg),
            ExportMode::TranslationOnly,
            &style,
        );
        let body = &ass_dialogue_texts(&content)[0];
        assert_eq!(body, "Hello world", "仅译文模式不得改动正文: {body}");

        let mut fallback = seg;
        fallback.translation = None;
        let content = write_ass_text(&[fallback], ExportMode::TranslationOnly, &style);
        let body = &ass_dialogue_texts(&content)[0];
        assert!(
            body.contains("\\kf100}你好"),
            "译文回退原文时应照常插标签: {body}"
        );
    }

    /// 双语导出：只在原文行（第二行）插标签，译文行一个标签都不插。
    #[test]
    fn ass_karaoke_bilingual_tags_source_line_only() {
        let mut seg = Segment::new(1, 0.0, 2.0, "你好世界");
        seg.translation = Some("Hello world".to_string());
        seg.words = vec![
            WordTiming {
                word: "你好".to_string(),
                start: 0.0,
                end: 1.0,
            },
            WordTiming {
                word: "世界".to_string(),
                start: 1.0,
                end: 2.0,
            },
        ];
        let content = write_ass_text(&[seg], ExportMode::Bilingual, &karaoke_test_style());
        let body = &ass_dialogue_texts(&content)[0];
        let (trans_line, source_line) = body.split_once("\\N").expect("双语应折成两行");

        assert_eq!(trans_line, "Hello world");
        assert!(!trans_line.contains("\\kf"), "译文行不得插标签: {body}");
        assert_eq!(karaoke_centiseconds(source_line), vec![100, 100]);
        assert!(
            source_line.contains("{\\1c&H0081B910&\\2c&H00FFFFFF&\\kf100}你好{\\kf100}世界"),
            "原文行应逐词插标签: {body}"
        );
    }

    /// 保留字符净化：标签只允许插在普通文本之间。紧贴 `\` / `{` / `}` / `,`
    /// 的边界一律不插标签，该词的时长并入上一段（时间不丢，只是跟着上一段一起点亮）。
    #[test]
    fn ass_karaoke_sanitizes_reserved_char_boundaries() {
        let style = karaoke_test_style();

        // ① 第二个词紧接一个字面反斜杠：`\` + `{` 会被当成转义序列，标签必须让开。
        let mut backslash = Segment::new(1, 0.0, 2.0, "a\\b");
        backslash.words = vec![
            WordTiming {
                word: "a\\".to_string(),
                start: 0.0,
                end: 1.0,
            },
            WordTiming {
                word: "b".to_string(),
                start: 1.0,
                end: 1.5,
            },
        ];
        let body =
            &ass_dialogue_texts(&write_ass_text(&[backslash], ExportMode::RawOnly, &style))[0];
        assert_eq!(
            karaoke_centiseconds(body),
            vec![150],
            "时长应并入上一段: {body}"
        );
        assert!(body.contains("}a\\b"), "原文不得被改动: {body}");

        // ② 第二个词以 `{` 开头（正文里会被转义成 `\{`）：标签让开，转义照旧。
        let mut braces = Segment::new(1, 0.0, 2.0, "费用{固定}");
        braces.words = vec![
            WordTiming {
                word: "费用".to_string(),
                start: 0.0,
                end: 0.5,
            },
            WordTiming {
                word: "{固定}".to_string(),
                start: 0.5,
                end: 1.5,
            },
        ];
        let body = &ass_dialogue_texts(&write_ass_text(&[braces], ExportMode::RawOnly, &style))[0];
        assert_eq!(karaoke_centiseconds(body), vec![150]);
        assert!(
            body.contains("费用\\{固定\\}"),
            "花括号必须保持转义: {body}"
        );

        // ③ 逗号紧贴词首（Dialogue 的字段分隔符）：同样让开。
        let mut comma = Segment::new(1, 0.0, 2.0, "你好,世界");
        comma.words = vec![
            WordTiming {
                word: "你好,".to_string(),
                start: 0.0,
                end: 0.6,
            },
            WordTiming {
                word: "世界".to_string(),
                start: 0.6,
                end: 1.0,
            },
        ];
        let body = &ass_dialogue_texts(&write_ass_text(&[comma], ExportMode::RawOnly, &style))[0];
        assert_eq!(karaoke_centiseconds(body), vec![100]);
        assert!(body.ends_with("你好,世界"), "文本不得被改动: {body}");
    }

    /// 折行 + 逐词标签：标签按折行**之后**的位置插入，不劈标签、不丢字；
    /// 去掉标签后与「没有词级数据」的同一份输出逐字节相同。
    #[test]
    fn ass_karaoke_survives_wrapping() {
        let text = "今天天气很好我们去公园散步然后回家吃饭";
        let mut seg = Segment::new(1, 0.0, 4.0, text);
        seg.words = text
            .chars()
            .enumerate()
            .map(|(i, c)| WordTiming {
                word: c.to_string(),
                start: i as f64 * 0.2,
                end: i as f64 * 0.2 + 0.2,
            })
            .collect();
        let style = SubtitleStyleConfig {
            max_chars_per_line: 8,
            ..SubtitleStyleConfig::default()
        };

        let body = &ass_dialogue_texts(&write_ass_text(
            std::slice::from_ref(&seg),
            ExportMode::RawOnly,
            &style,
        ))[0];

        assert!(body.contains("\\N"), "这条用例必须先触发折行: {body}");
        assert_eq!(
            karaoke_centiseconds(body).len(),
            text.chars().count(),
            "每个字都该有自己的标签: {body}"
        );
        assert!(
            karaoke_centiseconds(body).iter().all(|cs| *cs == 20),
            "厘秒值错误: {body}"
        );

        let mut plain = seg;
        plain.words.clear();
        let plain_body = &ass_dialogue_texts(&write_ass_text(
            std::slice::from_ref(&plain),
            ExportMode::RawOnly,
            &style,
        ))[0];
        assert_eq!(
            strip_override_blocks(body),
            *plain_body,
            "插标签不得改变文字与折行结果"
        );
    }

    /// 开关关闭：即便有词级数据，输出也与「没有词级数据」逐字节一致（老用户兼容开关）。
    #[test]
    fn ass_karaoke_disabled_matches_plain_output() {
        let mut seg = Segment::new(1, 0.0, 2.0, "今天天气很好");
        seg.words = vec![WordTiming {
            word: " 今天".to_string(),
            start: 0.0,
            end: 1.0,
        }];
        let off = SubtitleStyleConfig {
            karaoke_word_highlight: false,
            ..karaoke_test_style()
        };
        let on = karaoke_test_style();

        let mut plain = seg.clone();
        plain.words.clear();

        let with_words_off = write_ass_text(&[seg], ExportMode::RawOnly, &off);
        let plain_on = write_ass_text(&[plain], ExportMode::RawOnly, &on);
        assert!(
            !with_words_off.contains("\\kf"),
            "关闭时不得插标签: {with_words_off}"
        );
        assert_eq!(with_words_off, plain_on, "关闭开关 = 回到导入前的输出");
    }

    /// 句尾静音不摊给最后一个词：`\kf` 之和 = 各词自身时长之和，句尾（以及跨句）
    /// 的静音不产生任何标签，也不会被最后一个词吞掉。
    #[test]
    fn ass_karaoke_silence_is_not_folded_into_words() {
        let mut seg = Segment::new(1, 10.0, 12.0, "说完就走");
        seg.words = vec![
            WordTiming {
                word: "说完".to_string(),
                start: 10.0,
                end: 10.3,
            },
            WordTiming {
                word: "就走".to_string(),
                start: 10.3,
                end: 10.5,
            },
        ];
        let body = &ass_dialogue_texts(&write_ass_text(
            &[seg],
            ExportMode::RawOnly,
            &karaoke_test_style(),
        ))[0];
        assert_eq!(karaoke_centiseconds(body), vec![30, 20]);
        assert_eq!(
            karaoke_centiseconds(body).iter().sum::<u32>(),
            50,
            "1.5 秒句尾静音不得计入任何一个词: {body}"
        );
    }

    /// 对齐口径：容忍前导空格与后补的标点；顺序错 / 缺词 / 改字一律判为对不上。
    #[test]
    fn plan_karaoke_tags_alignment_rules() {
        let words = vec![
            WordTiming {
                word: " 今天".to_string(),
                start: 0.0,
                end: 1.0,
            },
            WordTiming {
                word: "天气".to_string(),
                start: 1.0,
                end: 1.5,
            },
            WordTiming {
                word: " 很好".to_string(),
                start: 1.5,
                end: 2.0,
            },
        ];
        // 标点恢复补的标点、识别器的前导空格都不影响对齐（偏移按字节算）。
        let plan = plan_karaoke_tags(&words, "今天，天气很好。").expect("应能对齐");
        assert_eq!(
            plan,
            vec![(0usize, 100u32), (9, 50), (15, 50)],
            "词首偏移与厘秒时长不对"
        );

        // 顺序错 / 缺词 / 改字：整句判为对不上。
        let reversed = vec![words[2].clone(), words[1].clone(), words[0].clone()];
        assert!(plan_karaoke_tags(&reversed, "今天天气很好").is_none());
        let missing = vec![WordTiming {
            word: "世界".to_string(),
            start: 0.0,
            end: 1.0,
        }];
        assert!(plan_karaoke_tags(&missing, "今天天气很好").is_none());
        let edited = vec![
            WordTiming {
                word: "今天".to_string(),
                start: 0.0,
                end: 1.0,
            },
            WordTiming {
                word: "世界".to_string(),
                start: 1.0,
                end: 2.0,
            },
        ];
        assert!(plan_karaoke_tags(&edited, "今天天气很好").is_none());

        // 纯空白词元与非有限时间的词只被跳过，不拖累整句。
        let noisy = vec![
            WordTiming {
                word: "  ".to_string(),
                start: 0.0,
                end: 0.1,
            },
            WordTiming {
                word: "今天".to_string(),
                start: 0.1,
                end: 0.2,
            },
            WordTiming {
                word: "天气".to_string(),
                start: 0.2,
                end: f64::NAN,
            },
        ];
        assert_eq!(
            plan_karaoke_tags(&noisy, "今天天气"),
            Some(vec![(0, 10)]),
            "空白词元与 NaN 只跳过，不拖累整句"
        );
    }

    // ── SBV / LRC / 带时间码纯文本 ──

    /// 造一句带（可选）译文的片段，省掉每处样板。
    fn bilingual(index: usize, start: f64, end: f64, text: &str, trans: &str) -> Segment {
        let mut s = Segment::new(index, start, end, text);
        s.translation = Some(trans.to_string());
        s.translation_lang = Some("English".to_string());
        s
    }

    /// SBV：时间轴一行、正文一行，毫秒用点号，逗号分隔起止。
    #[test]
    fn sbv_writes_timing_and_text_lines() {
        let segs = vec![
            Segment::new(1, 1.0, 2.5, "第一句"),
            Segment::new(2, 3.0, 4.0, "第二句"),
        ];
        let dir = tmp_dir("sbv");
        let out = dir.join("out.sbv");
        SubtitleWriter::write_to_file(&segs, &out, "sbv").unwrap();

        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "0:00:01.000,0:00:02.500\n第一句\n\n0:00:03.000,0:00:04.000\n第二句\n\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SBV 一条字幕只有一行：双语两行必须压成空格，否则播放器会把第二行当成
    /// 另一条字幕的正文，整条轨错位。
    #[test]
    fn sbv_flattens_bilingual_into_one_line() {
        let segs = vec![bilingual(1, 0.0, 2.0, "你好", "Hello")];
        let dir = tmp_dir("sbv_biling");
        let out = dir.join("out.sbv");
        SubtitleWriter::write_to_file_with_mode(&segs, &out, "sbv", ExportMode::Bilingual).unwrap();

        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text, "0:00:00.000,0:00:02.000\nHello 你好\n\n");
        // 三行：时间轴、正文、块末空行。正文里不能出现换行（否则会被当成另一条字幕）。
        assert_eq!(text.lines().count(), 3, "正文必须是单行: {text:?}");
        assert!(!text.contains("Hello'\n'"), "双语不能被折成两行: {text:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SBV 文本同样要转义 `<` `&`：播放器按 HTML 子集解析，未转义的 `<` 会吞文本。
    #[test]
    fn sbv_escapes_markup_entities() {
        let segs = vec![Segment::new(1, 0.0, 1.0, "a & b <tag>")];
        let dir = tmp_dir("sbv_esc");
        let out = dir.join("out.sbv");
        SubtitleWriter::write_to_file(&segs, &out, "sbv").unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("a &amp; b &lt;tag&gt;"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SBV 与 SRT 必须表示同一时刻：同一份输入只换分隔符，时间数值逐一相等。
    #[test]
    fn sbv_timestamps_match_srt_numerically() {
        assert_eq!(seconds_to_sbv_time(0.0), "0:00:00.000");
        assert_eq!(seconds_to_sbv_time(65.123), "0:01:05.123");
        assert_eq!(seconds_to_sbv_time(3661.5), "1:01:01.500");
        // 进位与钳位沿用 SRT 的实现：59.9995 进位成 1 分，100 小时钳到上限
        assert_eq!(seconds_to_sbv_time(59.9995), "0:01:00.000");
        assert_eq!(seconds_to_sbv_time(360_000.0), "99:59:59.999");
        // 负值与 NaN 不产出怪东西
        assert_eq!(seconds_to_sbv_time(-1.0), "0:00:00.000");
        assert_eq!(seconds_to_sbv_time(f64::NAN), "0:00:00.000");
    }

    /// LRC：每行 `[mm:ss.cc]正文`，只带起点时间（这是 LRC 自身的设计）。
    #[test]
    fn lrc_writes_start_time_and_text() {
        let segs = vec![
            Segment::new(1, 7.0, 9.0, "第一句"),
            Segment::new(2, 65.25, 66.0, "第二句"),
        ];
        let dir = tmp_dir("lrc");
        let out = dir.join("out.lrc");
        SubtitleWriter::write_to_file(&segs, &out, "lrc").unwrap();

        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "[00:07.00]第一句\n[01:05.25]第二句\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LRC 行首的 `[` 会被播放器当成时间标签开头，导致整行不显示，必须补一个空格。
    #[test]
    fn lrc_escapes_leading_bracket() {
        let segs = vec![Segment::new(1, 0.0, 1.0, "[掌声]谢谢大家")];
        let dir = tmp_dir("lrc_bracket");
        let out = dir.join("out.lrc");
        SubtitleWriter::write_to_file(&segs, &out, "lrc").unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text, "[00:00.00] [掌声]谢谢大家\n");
        assert!(
            !text.starts_with("[00:00.00]["),
            "行首方括号必须被隔开: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LRC 正文的首尾空白必须与导入侧同口径（都 trim）：否则「写→读→写」会来回横跳。
    #[test]
    fn lrc_trims_body_like_the_reader_does() {
        let segs = vec![Segment::new(1, 7.0, 9.0, "  第一句  ")];
        let dir = tmp_dir("lrc_trim");
        let out = dir.join("out.lrc");
        SubtitleWriter::write_to_file(&segs, &out, "lrc").unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "[00:07.00]第一句\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正文里的空行会被 SRT/VTT 的块分隔切开：导出前必须折掉，
    /// 否则「导出 → 导入」会丢掉空行之后的文本。
    #[test]
    fn srt_and_vtt_collapse_blank_lines_inside_text() {
        let segs = vec![Segment::new(1, 0.0, 2.0, "第一行\n\n第二行")];
        let dir = tmp_dir("blank_lines");
        let srt = dir.join("a.srt");
        let vtt = dir.join("a.vtt");
        SubtitleWriter::write_srt(&segs, &srt).unwrap();
        SubtitleWriter::write_to_file(&segs, &vtt, "vtt").unwrap();

        let srt_text = std::fs::read_to_string(&srt).unwrap();
        assert!(
            srt_text.contains("第一行\n第二行"),
            "空行必须折成单个换行: {srt_text:?}"
        );
        assert!(
            !srt_text.contains("\n\n\n"),
            "不应留下连续空行: {srt_text:?}"
        );
        let vtt_text = std::fs::read_to_string(&vtt).unwrap();
        assert!(
            vtt_text.contains("第一行\n第二行"),
            "VTT 同样要折: {vtt_text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 带时间码纯文本：`[00:01:23.456] 正文`，一行一句，换行压平。
    #[test]
    fn txt_with_timecodes_prefixes_each_line() {
        let segs = vec![
            Segment::new(1, 0.0, 1.0, "第一句"),
            bilingual(2, 83.456, 84.0, "第二句", "Second"),
        ];
        let dir = tmp_dir("txt_ts");
        let out = dir.join("out.txt");
        SubtitleWriter::write_to_file_with_mode(&segs, &out, "txt-ts", ExportMode::Bilingual)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "[00:00:00.000] 第一句\n[00:01:23.456] Second 第二句\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 纯 TXT 的语义不能被带时间码的格式顶掉：`.txt` 仍是一行一句、不带时间。
    #[test]
    fn plain_txt_stays_untouched_by_new_format() {
        let segs = vec![
            Segment::new(1, 1.0, 2.0, "第一句"),
            Segment::new(2, 3.0, 4.0, "第二句"),
        ];
        let dir = tmp_dir("txt_plain");
        let plain = dir.join("plain.txt");
        let stamped = dir.join("stamped.txt");
        SubtitleWriter::write_to_file(&segs, &plain, "txt").unwrap();
        SubtitleWriter::write_to_file(&segs, &stamped, "txt-ts").unwrap();

        assert_eq!(std::fs::read_to_string(&plain).unwrap(), "第一句\n第二句\n");
        assert_ne!(
            std::fs::read_to_string(&plain).unwrap(),
            std::fs::read_to_string(&stamped).unwrap(),
            "两种纯文本格式必须可区分"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `export_spec_for` 的新条目必须**真的**被 writer 接受，且扩展名与内容一致
    /// （否则会出现「文件叫 .sbv、内容是 SRT」这种静默不一致）。
    #[test]
    fn export_spec_covers_new_formats_and_writer_accepts_them() {
        for (fmt, ext) in [
            ("sbv", "sbv"),
            ("lrc", "lrc"),
            ("txt-ts", "txt"),
            ("txtts", "txt"),
        ] {
            let (spec_ext, label) = export_spec_for(fmt);
            assert_eq!(spec_ext, ext, "{fmt} 的扩展名应一致");
            assert!(!label.is_empty(), "{fmt} 必须有对话框过滤器名");

            let dir = tmp_dir(&format!("spec_{fmt}"));
            let out = dir.join(format!("out.{spec_ext}"));
            SubtitleWriter::write_to_file_with_mode(&sample(), &out, fmt, ExportMode::RawOnly)
                .unwrap_or_else(|e| panic!("{fmt} 未被 writer 接受: {e}"));
            assert!(
                !std::fs::read_to_string(&out).unwrap().trim().is_empty(),
                "{fmt} 导出内容为空"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 带样式入口对 SBV / 带时间码纯文本同样生效（按 `max_chars_per_line` 折行），
    /// 而 `max_chars_per_line == 0` 时必须与无样式入口逐字节一致。
    #[test]
    fn style_path_wraps_new_formats_and_is_byte_identical_when_zero() {
        let segs = vec![Segment::new(
            1,
            0.0,
            2.0,
            "今天天气很好，我们一起去公园散步，然后回家吃饭，明天再说。",
        )];
        let wrapping = SubtitleStyleConfig {
            max_chars_per_line: 8,
            ..SubtitleStyleConfig::default()
        };
        let zero = SubtitleStyleConfig {
            max_chars_per_line: 0,
            ..SubtitleStyleConfig::default()
        };

        for fmt in ["sbv", "txt-ts"] {
            let dir = tmp_dir(&format!("style_{fmt}"));
            let plain = dir.join(format!("plain.{fmt}"));
            let zeroed = dir.join(format!("zeroed.{fmt}"));
            let wrapped = dir.join(format!("wrapped.{fmt}"));

            SubtitleWriter::write_to_file(&segs, &plain, fmt).unwrap();
            SubtitleWriter::write_to_file_with_style(
                &segs,
                &zeroed,
                fmt,
                ExportMode::RawOnly,
                &zero,
            )
            .unwrap();
            SubtitleWriter::write_to_file_with_style(
                &segs,
                &wrapped,
                fmt,
                ExportMode::RawOnly,
                &wrapping,
            )
            .unwrap();

            assert_eq!(
                std::fs::read_to_string(&plain).unwrap(),
                std::fs::read_to_string(&zeroed).unwrap(),
                "{fmt}: max_chars == 0 必须与无样式导出逐字节一致"
            );
            // LRC 刻意不折行（一行一句是它的语义），因此只有 SBV 参与折行断言。
            if fmt == "sbv" {
                assert_ne!(
                    std::fs::read_to_string(&plain).unwrap(),
                    std::fs::read_to_string(&wrapped).unwrap(),
                    "{fmt}: 带样式应当真的折行"
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 章节导出：`write_chapters_file` 写出 YouTube 章节文本（首章强制 00:00），
    /// 走原子写盘（目录里不留 `.part`）；只拿得到 segments 的通用链路也能落兜底章节。
    #[test]
    fn chapters_export_writes_youtube_text() {
        let chapters = vec![
            crate::utils::chapters::Chapter {
                start_secs: 12.0,
                title: "正片开始".to_string(),
            },
            crate::utils::chapters::Chapter {
                start_secs: 95.0,
                title: "第二段".to_string(),
            },
        ];
        let dir = tmp_dir("chapters");
        let out = dir.join("chapters.txt");
        write_chapters_file(&chapters, &out).expect("章节导出应成功");

        let content = std::fs::read_to_string(&out).unwrap();
        assert_eq!(content, "00:00 正片开始\n01:35 第二段\n");

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留 .part: {leftovers:?}");

        // 格式名映射：扩展名是 txt，但写盘格式名必须仍是 chapters
        assert_eq!(export_spec_for("chapters").0, "txt");
        assert_eq!(canonical_write_format("youtube-chapters"), "chapters");

        // 没有 Chapter 列表、只有 segments 的链路：走兜底分组，照样产出可发布的章节
        let fallback = dir.join("fallback.txt");
        SubtitleWriter::write_to_file(&sample(), &fallback, "chapters").unwrap();
        let fallback_text = std::fs::read_to_string(&fallback).unwrap();
        assert!(
            fallback_text.starts_with("00:00 "),
            "兜底章节首行必须是 00:00: {fallback_text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
