//! 字幕片段数据结构

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub index: usize,
    /// 起始时间 (秒)
    pub start: f64,
    /// 结束时间 (秒)
    pub end: f64,
    /// 原始转写识别文本 (原声识别内容，直接声音是啥就是啥)
    pub text: String,
    /// 翻译字幕 (其他语言对中文的翻译，无翻译时为 None)
    #[serde(default)]
    pub translation: Option<String>,
    /// 译文的目标语言（与 `TRANSLATE_TARGET_LANGS` 同集合，如 "English"）。
    ///
    /// 为什么必须记住它：翻译是**可重入**的——用户可能先译成 English、再改译成
    /// 日本語。没有这个字段时，`translated_count()` 只看 `translation` 是否非空，
    /// 于是切换目标语言后旧译文会被当成「已完成」，界面显示 100% 却整篇是英文，
    /// 「开始翻译」按钮也因 `done == total` 直接拒绝执行。
    #[serde(default)]
    pub translation_lang: Option<String>,
    /// LLM 润色文本 (保留字段兼容)
    #[serde(default)]
    pub polished: String,
    /// 语种 (zh, en 等)
    pub language: Option<String>,
    /// 模型对该片段的平均对数置信度 (avg_logprob，越小越不可靠；旧记录/非 Whisper 引擎为 None)
    #[serde(default)]
    pub confidence: Option<f64>,
    /// 说话人编号 (0 起，展示时 +1 为「说话人 N」)。
    /// `None` 表示尚未做过说话人分离，或该段没有有效人声。
    #[serde(default)]
    pub speaker: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportMode {
    /// 仅音频原文字幕
    RawOnly,
    /// 仅翻译字幕 (若无翻译回退原文)
    TranslationOnly,
    /// 双语对照字幕 (第一行中文翻译，第二行音频原文)
    Bilingual,
}

impl Segment {
    pub fn new(index: usize, start: f64, end: f64, text: impl Into<String>) -> Self {
        Self {
            index,
            start,
            end,
            text: text.into(),
            translation: None,
            translation_lang: None,
            polished: String::new(),
            language: None,
            confidence: None,
            speaker: None,
        }
    }

    /// 显示文本：优先取润色/标点恢复后的文本，为空时回退原声识别文本。
    ///
    /// Stage 3 的标点恢复与 LLM 润色会把结果写进 `polished`，界面与导出都经这里
    /// 取文本，因此非空即代表「有更好的版本可用」。
    pub fn display_text(&self) -> &str {
        if self.polished.trim().is_empty() {
            &self.text
        } else {
            &self.polished
        }
    }

    /// 送去翻译的源文本。
    ///
    /// 用 [`Segment::display_text`]（优先标点恢复/润色结果）而非裸 `text`：
    /// 标点恢复是给模型补断句线索的关键，拿无标点的 `text` 去翻译等于主动丢掉
    /// 这些线索，长句尤其容易被译得断不开。同时把换行压成空格，避免一条字幕
    /// 在批量协议里被拆成两行、错位到相邻序号上。
    pub fn translate_source(&self) -> String {
        self.display_text().replace(['\r', '\n'], " ")
    }

    /// 译文是否**已经**是该目标语言的（用于增量翻译与「已完成」判定）。
    ///
    /// 只认精确匹配：目标语言清单是固定枚举（`TRANSLATE_TARGET_LANGS`），
    /// 不做模糊匹配——把 "English" 与 "英语" 视作同一语言的猜测一旦猜错，
    /// 就会静默跳过本该重译的句子。
    pub fn translation_matches(&self, target_lang: &str) -> bool {
        match (
            self.translation.as_deref(),
            self.translation_lang.as_deref(),
        ) {
            (Some(text), Some(lang)) => !text.trim().is_empty() && lang == target_lang,
            _ => false,
        }
    }

    /// 是否**已经带有**一条非空译文（忽略纯空白）。
    ///
    /// 全项目只在这里判定「有没有译文」：导出模式选择、`translated_count`、
    /// 界面「—」占位都调用它。散落各处的 `translation.is_some()` 会漏掉
    /// 「译文是空串 / 纯空白」的情况——引擎在解析失败时可能写进一个空串，
    /// 那种句子不该被算成「有译文」（双语导出会多出一行空白）。
    pub fn has_translation(&self) -> bool {
        self.translation
            .as_deref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false)
    }

    /// 说话人标签（1 起编号，供界面与导出使用）
    pub fn speaker_label(&self) -> Option<String> {
        self.speaker.map(|s| format!("说话人 {}", s + 1))
    }

    /// 带说话人前缀的文本（仅在该段有说话人标签时加前缀）
    pub fn text_with_speaker(&self, mode: ExportMode) -> String {
        let body = self.export_text(mode);
        match self.speaker_label() {
            Some(label) if !body.trim().is_empty() => format!("{label}: {body}"),
            _ => body,
        }
    }

    /// 根据导出模式格式化字幕文本。
    ///
    /// 原文一律走 [`Segment::display_text`]（优先润色文本）；`Bilingual` 的第二行是
    /// 译文，保持不动。
    pub fn export_text(&self, mode: ExportMode) -> String {
        match mode {
            ExportMode::RawOnly => self.display_text().to_string(),
            ExportMode::TranslationOnly => match self.translation.as_deref() {
                // 空串 / 纯空白不算译文：回退原文，避免导出一片空白行
                Some(t) if !t.trim().is_empty() => t.to_string(),
                _ => self.display_text().to_string(),
            },
            ExportMode::Bilingual => match self.translation.as_deref() {
                // 同上：只有**非空**译文才拼出第二行，否则整段回落为纯原文
                Some(t) if !t.trim().is_empty() => {
                    format!("{}\n{}", t, self.display_text())
                }
                _ => self.display_text().to_string(),
            },
        }
    }

    /// 片段时长 (秒)
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }

    /// 剪辑工程（剪映 / FCPXML / Premiere）导出时用的**单行**文本。
    ///
    /// 工程文件里的字幕是「一条轨道项 = 一行文字」，与 SRT 的 [`ExportMode::Bilingual`]
    /// （两行叠在一个字幕块里）不同。因此这里把双语压成**一行**，且**沿用与
    /// [`Segment::export_text`] 一致的顺序：译文在前、原文在后**，避免同一份工程
    /// 换格式导出后上下行颠倒：
    /// - [`ExportMode::RawOnly`] → 原文；
    /// - [`ExportMode::TranslationOnly`] → 仅译文（译文为空时退回原文，避免空条）；
    /// - [`ExportMode::Bilingual`] → `译文  原文`（无译文时退回原文）。
    ///
    /// 为什么必须提供它：此前三个工程导出器都只写 `display_text()`，用户辛苦译好的
    /// 字幕在剪映 / 达芬奇 / Premiere 里**凭空消失**，而界面里明明看得见。
    pub fn project_export_text(&self, mode: ExportMode) -> String {
        let raw = self.display_text().replace(['\r', '\n'], " ");
        let trans = self
            .translation
            .as_deref()
            .unwrap_or("")
            .replace(['\r', '\n'], " ");
        let has_trans = !trans.trim().is_empty();
        match mode {
            ExportMode::RawOnly => raw,
            ExportMode::TranslationOnly if has_trans => trans,
            ExportMode::TranslationOnly => raw,
            ExportMode::Bilingual if has_trans => format!("{trans}  {raw}"),
            ExportMode::Bilingual => raw,
        }
    }
}

/// 术语表合规检查：返回**疑似未遵守术语表**的字幕下标。
///
/// 判定：某句原文里出现了术语的「原文」（`from`），但它的译文里**没有**出现对应的
/// 「译文」（`to`）。这类句子多半是模型没按术语表翻译，值得用户复核。
///
/// # 为什么是「疑似」而不是「一定错」
///
/// 自然的语言里，术语在译文里未必逐字原样出现（形态、大小写、单复数、语序都可能变），
/// 而且术语本身可能并不需要逐条落进每一句。所以这里只是**提示复核**，绝不自动改译文。
/// 为避免明显误报：
/// - 只检查**已经带译文**的句子；
/// - `from` 与 `to` 都去掉首尾空白后再比对；
/// - 大小写不敏感（ASCII）。
///
/// 只返回**去重后的下标**，顺序与原句一致，供界面标注与统计。
pub fn glossary_violations(segments: &[Segment], entries: &[(String, String)]) -> Vec<usize> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for seg in segments {
        let Some(trans) = seg.translation.as_deref() else {
            continue;
        };
        if trans.trim().is_empty() {
            continue;
        }
        let source = seg.translate_source().to_lowercase();
        let translated = trans.to_lowercase();
        let violated = entries.iter().any(|(from, to)| {
            let from = from.trim().to_lowercase();
            let to = to.trim().to_lowercase();
            !from.is_empty()
                && !to.is_empty()
                && source.contains(&from)
                && !translated.contains(&to)
        });
        if violated {
            out.push(seg.index);
        }
    }
    out
}

/// 低置信判定的默认阈值（`avg_logprob`，越小越不可靠）。
///
/// # 依据
///
/// 1. **与自动救场同口径**。`core/pipeline.rs::plan_rescue_spans` 用
///    `confidence < threshold` 挑二段重解码窗口，阈值来自
///    `PipelineConfig::whisper_rescue_logprob`（`config.toml` 当前为 `0.0`＝关闭）。
///    给用户看的复核清单若另起一套数值语义，会出现「管线判它没问题、界面却标红」
///    的自相矛盾，因此这里沿用同一条 `avg_logprob` 越小越可疑的判据，
///    只是复核清单必须**先给一个能用的默认值**（救场的默认是「关」，不能拿来做界面阈值）。
/// 2. **真实分布实测**。本机 `ggml-small-q5_0` + Silero VAD 0.50、`-ojf` 全量 token 概率，
///    对 `testVideo/03.1.3概率不等式.mp4` 的 05:05–10:05 真实片段自算 `avg_logprob`
///    （与 `WhisperEngine::tokens_avg_logprob` 同一算法，过滤 `[_BEG_]` 类特殊 token）：
///    - 190 句全部有置信度，区间 `[-0.673, -0.004]`，均值 -0.171、中位数 -0.139；
///    - 分位：p10 -0.350、p25 -0.226、p75 -0.062、p90 -0.021；
///    - 命中比例：`< -0.20` → 31.6%、`< -0.35` → 10.0%、`< -0.50` → 4.2%、
///      `< -0.65` → 1.6%；**没有任何一句低于 -0.75**。
/// 3. **取 -0.35**。它落在实测 p10 上，约 10% 的句子进复核清单——「一键定位 + 人工过一遍」
///    是用户能真的做完的工作量；更松（如 -0.20）要把三成句子标黄，清单会被淹掉；
///    更紧（如 -0.65）只剩 1.6%，等于放过绝大多数可改进的句子。
///    注意本样本是**纯讲课、无背景音乐**的干净音频；含噪素材整条分布会整体下移，
///    所以这里不再往松的方向留余量（宁可多标，评测量级上 10% 仍可人工消化）。
/// 4. 与 `pipeline.rs` 里的 `0.35` 数值相同**纯属巧合**：那是「低置信片段的**时长占比**
///    超过 35% 就放弃救场」的上限，与 `avg_logprob` 无关，别当成同一个阈值。
///
/// TODO(接入配置)：位于 `src/utils/config.rs` 的 `PipelineConfig` 才是它该待的地方。
/// 建议落在 `whisper_rescue_logprob` 旁边（`config.rs:544-547` 附近），字段形如
/// `pub whisper_low_confidence: f64`（`#[serde(default = "default_low_confidence")]`，
/// 默认 -0.35），由界面暴露滑杆；本线程无权改 `config.rs`，故先固化成具名常量。
pub const DEFAULT_LOW_CONFIDENCE_THRESHOLD: f64 = -0.35;

/// 空/超短句判据：`display_text()` 去空白后字符数**少于**该值即视为 ASR 碎片噪声。
///
/// `optimize_segments` 已经剔掉了空文本与时长 <= 0.05s 的无效片段，但「单字碎音」
/// （「嗯」「啊」）与标点恢复前的孤立字仍会留下：它们在字幕里几乎没有信息量，
/// 却会明显拉低观感，值得进复核清单。取 2 而不是 1——单个汉字（「是」「对」）
/// 本身可能就是一句完整回答，只按 1 个字符判会把正常短答也标出来。
pub const MIN_SEGMENT_TEXT_CHARS: usize = 2;

/// 一次转写质检的结果。
///
/// 四个判据各自持有一串**句序号**（`Segment::index`）。这是与
/// `AppState::select_segment(index)` 同一套编号，因此可以原样拿去定位——
/// 不需要再做「列表下标 ↔ 句序号」的换算（`glossary_violations` 也是返回 `seg.index`）。
///
/// # 同一句命中多条时**不去重**
///
/// 一个句子可以同时「低置信 + 术语违规 + 超短」，这三条是**互相独立**的缺陷：
/// 去重会丢掉「这句有两个毛病」的信息，也会让每个判据的计数与它自己列表的长度对不上。
/// 所以分类列表各自保留该句、允许交叉。需要「一句只看一次」的地方（一键定位、
/// 「下一处」这类按句推进的复核动作）走 [`QualityReport::all_issues`]，
/// 由它做合并 + 升序 + 去重。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QualityReport {
    /// 低置信：`confidence` 有值且**严格小于**阈值。等于阈值不算（与
    /// `plan_rescue_spans` 的 `c < threshold` 一致）。`None` **不计入**——见 [`quality_report`]。
    pub low_confidence: Vec<usize>,
    /// 疑似未遵守术语表（原样转发 [`glossary_violations`]，与剪辑台琥珀色提示同源）。
    pub glossary_violations: Vec<usize>,
    /// 未翻译：原文非空但 [`Segment::has_translation`] 为假。
    /// 仅在调用方要求检查时非空——整篇未翻译是「还没开始」而不是缺陷。
    pub untranslated: Vec<usize>,
    /// 空句或超短句：`display_text()` 去空白后为空，或字符数 < [`MIN_SEGMENT_TEXT_CHARS`]。
    pub empty_or_short: Vec<usize>,
    /// 有多少句**带**置信度（界面上用来交代低置信判据的覆盖面）。
    pub confidence_scored: usize,
    /// 有多少句**没有**置信度（SenseVoice / 旧记录 / 流式预览片段）。
    ///
    /// 这个数字是本次质检的**能力缺口**：这些句子**没有**被算成低置信
    /// （`None` 既不是「可靠」也不是「不可靠」），所以 `low_confidence` 对它们恒为空，
    /// SenseVoice 用户拿不到低置信复核。
    pub confidence_missing: usize,
}

impl QualityReport {
    /// 四个判据的计数之和。同一句命中多条会被重复计数——它是「待复核**条目**数」，
    /// 不是「有问题的**句子**数」（后者看 [`QualityReport::all_issues`] 的长度）。
    pub fn total_issues(&self) -> usize {
        self.low_confidence.len()
            + self.glossary_violations.len()
            + self.untranslated.len()
            + self.empty_or_short.len()
    }

    /// 是否一条待复核都没有。
    pub fn is_clean(&self) -> bool {
        self.total_issues() == 0
    }

    /// 合并四个判据的句序号，升序去重。供「一键定位」「下一处」这类按句推进的动作使用。
    pub fn all_issues(&self) -> Vec<usize> {
        let mut all: Vec<usize> = self
            .low_confidence
            .iter()
            .chain(&self.glossary_violations)
            .chain(&self.untranslated)
            .chain(&self.empty_or_short)
            .copied()
            .collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    /// 第一处待复核的句序号（无问题时 `None`）。这就是「一键定位」的落点。
    pub fn first_issue(&self) -> Option<usize> {
        self.all_issues().into_iter().next()
    }

    /// 置信度是否**完全**缺席：此时低置信判据对所有句子都不可用，界面应说明原因
    /// 而不是显示「低置信 0 句」让用户误以为这段音频很干净。
    pub fn confidence_unavailable(&self) -> bool {
        self.confidence_scored == 0 && self.confidence_missing > 0
    }
}

/// 对一批字幕跑一次质检，产出可单测的纯报告（不碰界面、不碰 IO、不做任何修改）。
///
/// - `confidence_threshold`：`avg_logprob` 下限，**严格小于**才判低置信；
///   传 [`DEFAULT_LOW_CONFIDENCE_THRESHOLD`] 即产品默认值。
/// - `check_untranslated`：是否把「有原文无译文」算作缺陷。由**调用方**决定，
///   因为整篇未翻译时那不是缺陷而是「还没开始翻译」（见 [`QualityReport::untranslated`]）。
///
/// # `confidence` 为 `None` 的取舍
///
/// `None` **不计入** `low_confidence`，只累加到 `confidence_missing`。
///
/// 理由：`None` 的语义是「这条链路根本不产生逐句置信度」，而不是「不可靠」：
/// - SenseVoice 两条构造路径都写死 `confidence: None`
///   （`src/engines/sensevoice.rs:492`、`:529`）；
/// - Whisper 自己也只在 `-ojf`（救场开启）时才有 token 概率
///   （`src/engines/whisper.rs:945` 是唯一填充点，`:803` 的流式预览与 `:1047` 的
///   stdout 回退同样是 `None`）。
///
/// 若把 `None` 一律当低置信，SenseVoice 用户会看到**满屏标红**（几千句全命中），
/// 复核清单直接失去意义、还被误当成真缺陷；反之若静默忽略，用户会把
/// 「低置信 0 句」读成「质检通过」。所以这里显式统计进 `confidence_missing`，
/// 由界面提示「当前引擎不提供逐句置信度」——即记录能力缺口，而不是伪造判据结果。
pub fn quality_report(
    segments: &[Segment],
    entries: &[(String, String)],
    confidence_threshold: f64,
    check_untranslated: bool,
) -> QualityReport {
    let mut report = QualityReport::default();
    for seg in segments {
        match seg.confidence {
            Some(c) => {
                report.confidence_scored += 1;
                if c < confidence_threshold {
                    report.low_confidence.push(seg.index);
                }
            }
            None => report.confidence_missing += 1,
        }

        let text_len = seg.display_text().trim().chars().count();
        if check_untranslated && text_len > 0 && !seg.has_translation() {
            report.untranslated.push(seg.index);
        }
        if text_len < MIN_SEGMENT_TEXT_CHARS {
            report.empty_or_short.push(seg.index);
        }
    }

    // 术语违规直接复用现成判据（只查已带译文的句子），保证与剪辑台的琥珀色提示同源：
    // 同一份数据在转写页与剪辑页给出同一个数字，不会「这页 3 句、那页 5 句」。
    report.glossary_violations = glossary_violations(segments, entries);
    report
}

/// 便捷入口：不检查未翻译、也不看术语表，只按置信度与碎片句给出基础报告。
///
/// 保留它的理由：完整形态的 [`quality_report`] 需要调用方提供术语表条目与
/// 「是否检查未翻译」的开关，而这两样都由界面层掌握（术语表在 `AppState`，
/// 未翻译与否取决于用户是否已经翻译过）。只想要低置信 / 碎片句这两项的调用点
/// 与单测，不必先拼一份空术语表。
pub fn quality_report_basic(segments: &[Segment], confidence_threshold: f64) -> QualityReport {
    quality_report(segments, &[], confidence_threshold, false)
}

/// 这一批字幕里是否存在说话人标签。
///
/// 导出时以「整批」为单位决定是否加前缀：只有部分行带前缀会看起来像漏标，
/// 所以只要有任意一句带标签，就统一加。
pub fn has_speaker_labels(segments: &[Segment]) -> bool {
    segments.iter().any(|s| s.speaker.is_some())
}

/// 把 ISO-639-1 语言码转成提示词里用的可读语言名；未知码原样返回（去掉空白）。
///
/// 翻译提示词此前只告诉模型**目标语言**，从不提源语言。对「中→英」这类常见方向
/// 影响不大，但遇到「日语→中文」「中英混排→英文」时，模型少了这个线索容易漏译
/// 或把不该翻的专名翻掉。这里把 ASR 已经检测到的语言（`Segment::language`）转成
/// 可读名喂给提示词，作为一条轻量线索。
pub fn language_name(code: &str) -> String {
    match code.trim().to_ascii_lowercase().as_str() {
        "zh" | "zh-cn" | "zh-hans" | "cmn" => "中文",
        "zh-tw" | "zh-hant" => "繁体中文",
        "en" => "英语",
        "ja" | "jp" => "日语",
        "ko" => "韩语",
        "ru" => "俄语",
        "fr" => "法语",
        "de" => "德语",
        "es" => "西班牙语",
        "it" => "意大利语",
        "pt" => "葡萄牙语",
        "yue" => "粤语",
        other => return other.to_string(),
    }
    .to_string()
}

/// 这一批字幕里**最常见**的非空语言码（用于给翻译提示词提供源语言线索）。
///
/// 取众数而非「第一条」：批次可能跨越语种切换，但绝大多数情况下整批同语种，
/// 众数比首条更稳。全为空时返回 `None`（此时提示词不带源语言线索）。
///
/// 接受 `&[&Segment]`（调用方常先筛出非空片段）或 `&[Segment]` 等任意片段迭代器。
pub fn dominant_language<'a, I>(segments: I) -> Option<String>
where
    I: IntoIterator<Item = &'a Segment>,
{
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for seg in segments {
        if let Some(lang) = seg
            .language
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
        .map(|(lang, _)| lang.to_string())
}

/// 按导出模式取文本，并按需带上说话人前缀。
pub fn export_text_for(seg: &Segment, mode: ExportMode, with_speaker: bool) -> String {
    if with_speaker {
        seg.text_with_speaker(mode)
    } else {
        seg.export_text(mode)
    }
}

/// 智能优化字幕时间轴与消除鬼影/闪烁：
/// 1. 按起始时间升序排序
/// 2. 剔除无效片段 (duration <= 0.05s) 以及模型幻读重叠鬼影 (< 0.25s 且与后句同时间启动)
/// 3. 消除时间戳倒退与重叠冲突（前句尾部不超出后句头部）
/// 4. 长句自动拆分：超过 6 秒 / 60 字的大句按标点就近均分，时间按字数比例分配
/// 5. 用第 2 步同一判据复扫一遍：消重叠/拆分都只动边界，可能在本次造出新的无效或鬼影片段
/// 6. 广播级极短句平滑：短句 (< 0.8s) 在间隙允许范围内适当延展，避免 0.1s~0.4s 闪烁过快导致人眼无法阅读
/// 7. 重新编排连续序号 (1, 2, 3...)
///
/// **必须幂等**：`AppState` 每次载入工程（`load_task_with`、`load_from_cache`）都会调用本函数，
/// 不幂等就表现为「同一个工程打开一次 N 条、再打开少一条」，而且消失的那条文本无处可寻。
pub fn optimize_segments(segments: &mut Vec<Segment>) {
    if segments.is_empty() {
        return;
    }

    // 1. 排序
    segments.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 2. 剔除无效时长与同时间戳鬼影片段
    let mut filtered = Vec::with_capacity(segments.len());
    for i in 0..segments.len() {
        // 检查是否为同时间段重叠的极短鬼影碎片 (例如 100ms 的 "对吧？" 与紧随其后同一毫秒启动的整句)
        if should_drop_segment(&segments[i], segments.get(i + 1)) {
            // 无效片段 / 模型分词切片边界重复，跳过
            continue;
        }
        filtered.push(segments[i].clone());
    }

    // 3. 消除时间重叠
    for j in 0..filtered.len().saturating_sub(1) {
        if filtered[j].end > filtered[j + 1].start {
            if filtered[j + 1].start > filtered[j].start {
                filtered[j].end = (filtered[j].start + 0.1).max(filtered[j + 1].start);
            } else {
                filtered[j + 1].start = filtered[j].end;
            }
        }
    }

    // 4. 长句自动拆分：转写初始阶段就把 8 秒级大句按标点切短，句长可控且画面字幕不超屏
    let filtered = split_long_segments(filtered);

    // 5. 复扫：第 3 步消重叠只改边界，可能把前句尾部收到 `start + 0.1`，
    // 造出一条「时长过短 + 与后句几乎同时开始」的鬼影碎片；第 4 步拆分也可能
    // 留下类似的短碎片。若留到下一次载入才由第 2 步剔除，用户看到的就是
    // 「点开一次两条、再点开少一条」，而且那条碎条的文本被静默丢弃。
    // 这里用与第 2 步完全相同的判据复扫到稳定（复扫只会删、不会改边界）。
    //
    // 必须排在**平滑之前**：平滑会把这类碎条的尾部往 `next.start - 0.04` 推，
    // 一旦推得比「本句起点 + 0.15」还远，它就**不再是鬼影**而被保留下来——
    // 于是同一个工程第一次载入留下 N 条、第二次载入（此时碎条已消失、平滑
    // 的邻居与可延展空间都变了）得到 N-1 条，且两次的 `end` 也不相同。
    // 先复扫再平滑，则平滑的输入是稳定集合，两次载入逐字节一致。
    let mut filtered = drop_invalid_and_ghosts_until_stable(filtered);

    // 6. 极短句平滑延展 (最低停留 0.8 秒，若有空隙则延长显示，防止字闪)
    const MIN_READABLE_DUR: f64 = 0.8;
    let n = filtered.len();
    for j in 0..n {
        let dur = filtered[j].end - filtered[j].start;
        if dur < MIN_READABLE_DUR {
            let next_start = if j + 1 < n {
                filtered[j + 1].start
            } else {
                filtered[j].start + 2.0
            };
            if next_start > filtered[j].end {
                // 留出 40ms 呼吸空隙给下一句，或延满 0.8s
                let max_target = (next_start - 0.04).max(filtered[j].end);
                let desired = filtered[j].start + MIN_READABLE_DUR;
                filtered[j].end = desired.min(max_target);
            }
        }
    }

    // 7. 重新编排序号
    for (idx, seg) in filtered.iter_mut().enumerate() {
        seg.index = idx + 1;
    }

    *segments = filtered;
}

/// 「无效或鬼影」判据：单条片段是否应在筛除阶段丢弃。
///
/// 合并成一处是刻意的：[`optimize_segments`] 的第 2 步首筛与第 6 步复扫必须用同一条
/// 判据，否则首筛放过的碎片会在复扫时被删（或反过来），函数就不再幂等。
/// `next` 是紧随其后的片段；为 `None`（末条）时不做鬼影判断。
fn should_drop_segment(seg: &Segment, next: Option<&Segment>) -> bool {
    // 无效时长（含被压成 0 甚至负长度的区间）或空文本
    if seg.end - seg.start <= 0.05 || seg.display_text().trim().is_empty() {
        return true;
    }
    // 同时间段重叠的极短鬼影碎片：例如消重叠后被压到 0.1s、又与后句起点只差 50ms
    match next {
        Some(n) => {
            seg.end - seg.start < GHOST_MAX_DUR && (n.start - seg.start).abs() < GHOST_START_TOL
        }
        None => false,
    }
}

/// 反复用 [`should_drop_segment`] 过滤，直到一轮下来不再删任何片段。
///
/// 为什么要迭代而不是单趟扫描：删掉一条之后，它前一句的「后一句」就换成了更后面的一条，
/// 判据的邻居随之改变，新暴露出来的鬼影只有再扫一遍才能发现。每轮要么删掉至少一条、
/// 要么直接收敛，因此必然终止；每轮是 O(n) 的一次遍历（用 `into_iter` 移动而非克隆），
/// 只在载入/转写收尾跑一次。
fn drop_invalid_and_ghosts_until_stable(segments: Vec<Segment>) -> Vec<Segment> {
    let mut current = segments;
    loop {
        let len = current.len();
        let mut kept: Vec<Segment> = Vec::with_capacity(len);
        let mut rest = current.into_iter().peekable();
        while let Some(seg) = rest.next() {
            if should_drop_segment(&seg, rest.peek()) {
                continue;
            }
            kept.push(seg);
        }
        if kept.len() == len {
            return kept;
        }
        current = kept;
    }
}

/// 长句自动拆分阈值：单条字幕最长显示 6 秒 / 60 字
const MAX_SEGMENT_DUR: f64 = 6.0;
const MAX_SEGMENT_CHARS: usize = 60;

/// 可作为长句切分点的中英文标点
fn is_split_punct(c: char) -> bool {
    matches!(
        c,
        '，' | '。' | '！' | '？' | '；' | '、' | '：' | '…' | ',' | '.' | '!' | '?' | ';' | ':'
    )
}

/// 在 text 中寻找最接近 ratio 位置的标点切点（切点标点归前段）。
/// 切点强制落在 25%~75% 区间且两侧各保留至少 2 字符，避免切出头尾碎渣；无合理切点返回 None。
fn split_index_at_ratio(text: &str, ratio: f64) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n < 4 {
        return None;
    }
    let target = (n as f64 * ratio.clamp(0.0, 1.0)) as usize;
    let lo = ((n as f64) * 0.25) as usize;
    let hi = ((n as f64) * 0.75).ceil() as usize;
    let mut best: Option<(usize, i64)> = None;
    for (i, &c) in chars.iter().enumerate() {
        if !is_split_punct(c) {
            continue;
        }
        let left_len = i + 1;
        if left_len < 2 || n - left_len < 2 || left_len < lo || left_len > hi {
            continue;
        }
        let dist = (left_len as i64 - target as i64).abs();
        if best.is_none_or(|(_, best_dist)| dist < best_dist) {
            best = Some((left_len, dist));
        }
    }
    best.map(|(cut, _)| cut)
}

/// 把 `text` 按 `ratio` 切成前后两半，供译文/润色文本跟随原文切点。
///
/// 优先在标点处切（与原文同一套就近规则）；找不到可用标点时按比例硬切，
/// **绝不让后半段落空**——否则右半段会退化成「没有译文」（双语导出时右行只剩
/// 原文，整句译文全挂在左行，双语行错位）或「没有润色」（展示/导出回落成未标点、
/// 未清理的原始识别文本，左右两半风格不一致）。
/// 文本长度不足 2 无法切分时返回 `None`，由调用方按「不切」处理。
fn split_following(text: &str, ratio: f64) -> (String, Option<String>) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n <= 1 {
        return (text.to_string(), None);
    }
    let cut = split_index_at_ratio(text, ratio).unwrap_or_else(|| {
        let c = (n as f64 * ratio.clamp(0.0, 1.0)).round() as usize;
        c.clamp(1, n - 1)
    });
    let left: String = chars[..cut].iter().collect();
    let right: String = chars[cut..].iter().collect();
    (left, Some(right))
}

/// 递归拆分单条长句：按标点就近均分文本，时间按字数比例分配；
/// 翻译与润色文本按同比例就近标点跟随拆分。无合理标点切点时保留原句。
fn split_segment_recursive(seg: Segment, out: &mut Vec<Segment>) {
    let char_count = seg.text.chars().count();
    if seg.duration() <= MAX_SEGMENT_DUR && char_count <= MAX_SEGMENT_CHARS {
        out.push(seg);
        return;
    }

    let Some(cut) = split_index_at_ratio(&seg.text, 0.5) else {
        out.push(seg);
        return;
    };

    let total = char_count as f64;
    let ratio = cut as f64 / total;
    let split_t = seg.start + (seg.end - seg.start) * ratio;
    let chars: Vec<char> = seg.text.chars().collect();
    let left_text: String = chars[..cut].iter().collect();
    let right_text: String = chars[cut..].iter().collect();

    let (left_trans, right_trans) = match &seg.translation {
        Some(trans) => {
            let (l, r) = split_following(trans, ratio);
            (Some(l), r)
        }
        None => (None, None),
    };

    let (left_polished, right_polished) = if seg.polished.is_empty() {
        (String::new(), String::new())
    } else {
        let (l, r) = split_following(&seg.polished, ratio);
        (l, r.unwrap_or_default())
    };

    let left = Segment {
        index: 0,
        start: seg.start,
        end: split_t,
        text: left_text,
        translation: left_trans,
        translation_lang: seg.translation_lang.clone(),
        polished: left_polished,
        language: seg.language.clone(),
        confidence: seg.confidence,
        speaker: seg.speaker,
    };
    let right = Segment {
        index: 0,
        start: split_t,
        end: seg.end,
        text: right_text,
        translation: right_trans,
        translation_lang: seg.translation_lang.clone(),
        polished: right_polished,
        language: seg.language,
        confidence: seg.confidence,
        speaker: seg.speaker,
    };
    split_segment_recursive(left, out);
    split_segment_recursive(right, out);
}

/// 逐条长句拆分：把超过 [`MAX_SEGMENT_DUR`] 秒 / [`MAX_SEGMENT_CHARS`] 字的片段按标点
/// 就近递归拆短（时间按字数比例分配，译文/润色/说话人随切点跟随）。
///
/// [`optimize_segments`] 内部第 4 步会调用它，因此**最终落库/导出的片段本来就已拆短**。
/// 公开出来是给转写期的**流式预览**用：VAD 段在密集讲话时可能长达 30s+（静音边界缺失），
/// 模型会把整段吐成一条，若原样推进 `streaming_segments`，界面上就会出现「一条字幕塞
/// 一整段话」，而结束后才被 `optimize_segments` 拆开——实时流与最终结果对不上。
/// 在推流收口处调用本函数，让预览粒度与最终结果一致（与 SenseVoice 侧
/// `tools/sensevoice_runner.py::split_sentence_timed` 同一策略）。
pub fn split_long_segments(segments: Vec<Segment>) -> Vec<Segment> {
    let mut out = Vec::with_capacity(segments.len());
    for seg in segments {
        split_segment_recursive(seg, &mut out);
    }
    out
}

/// 单条字幕的最短显示时长。微调与重叠修复都以此为准：压到 0 长度的字幕既看不见，
/// 又会被 [`optimize_segments`] 当成鬼影碎片剔除，用户会以为「改一下时间字幕就没了」。
pub const MIN_SEGMENT_DUR: f64 = 0.1;

/// [`optimize_segments`] 判定「幻读鬼影」的时长阈值：**短于**它的片段，只有在与后句
/// 几乎同时开始时才会被删掉。提成常量是为了让 [`plan_time_edit`] 与判据同源——
/// 两边各写一份数字，改一处就会重新出现「用户调完时间，下次载入丢句」。
const GHOST_MAX_DUR: f64 = 0.25;

/// 鬼影判定里的「几乎同时开始」容差（秒）。
const GHOST_START_TOL: f64 = 0.15;

/// 微调后允许的最短时长。
///
/// 为什么不用 [`MIN_SEGMENT_DUR`]（0.1s）：那只是「看得见」的下限，而
/// [`optimize_segments`] 会把「时长 < 0.25s 且与后句起点相差 < 0.15s」的片段当作
/// 幻读碎片直接删除。微调若把片段压进这个组合，用户下次载入工程就会丢句，且怎么
/// 改都回不来。取鬼影时长阈值本身作下限后，「本句时长 >= 0.25s」就与邻居起点差
/// 无关地免疫了这条判据，不必再去推算邻居的位置。
///
/// 公开给上层（如 `AppState::split_selected_segment`）是为了让「拆出来的两半
/// 也不能短于这个下限」这一条用同一个数字，避免两处各写一份而重新长出丢句现象。
pub const MIN_EDIT_DUR: f64 = GHOST_MAX_DUR;

/// 字幕清单的搜索过滤：返回命中的片段下标（关键字为空即全部下标）。
///
/// 抽成纯函数是为了让「编辑之后这串下标仍然覆盖全部字幕」能脱离 GPUI 直接单测。
/// 清单是虚拟列表、按这串下标取行，一旦下标漏项或越界，列表会因为量不到行高
/// （`item_height = 0`）而整片空白，而且只要缓存键不变就一直刷不出来。
pub fn matched_indices(segments: &[Segment], needle: &str) -> Vec<usize> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return (0..segments.len()).collect();
    }
    segments
        .iter()
        .enumerate()
        .filter(|(_, seg)| {
            seg.display_text().to_lowercase().contains(&needle)
                || seg
                    .translation
                    .as_deref()
                    .map(|t| t.to_lowercase().contains(&needle))
                    .unwrap_or(false)
        })
        .map(|(pos, _)| pos)
        .collect()
}

/// 下标序列是否仍可安全用于当前片段表：过滤态下允许少于片段总数，
/// 但任何一个下标都必须落在表内（越界即会让虚拟列表整片空白）。
pub fn indices_cover_segments(indices: &[usize], segment_count: usize) -> bool {
    indices.iter().all(|&i| i < segment_count)
}

/// 一次「微调」的纯计算结果：自身新区间 + 被越界时一并让位的邻居边界。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeEdit {
    pub start: f64,
    pub end: f64,
    /// 前一句被收短的终点（`None` 表示前一句不用动）
    pub prev_end: Option<f64>,
    /// 后一句被后挪的起点（`None` 表示后一句不用动）
    pub next_start: Option<f64>,
}

/// 规划一次时间微调：起点加 `d_start`、终点加 `d_end`。
///
/// 除了「起点 >= 0、终点比起点晚至少 [`MIN_EDIT_DUR`]」这两条自身约束，还要
/// **保证调完之后的时间轴能原样通过 [`optimize_segments`]**：
/// * 越过前一句的终点就把前一句的尾巴收回来；
/// * 越过下一句的起点就把下一句的头往后挪；
/// * 谁都让不开时，宁可拒绝这次微调（把本句的起点收回来），也不留下非法区间。
///
/// 为什么必须在这里挡住：交叉区间会让「按播放时间取句」取到错的那一句、时间轴块
/// 互相压盖；更要命的是下次载入工程时 [`optimize_segments`] 会按「排序 + 截断重叠 +
/// 剔除鬼影」把用户手工调好的时间再改一遍，用户看到的就是「怎么改都恢复不了」。
///
/// 给邻居留的余量是 [`MIN_EDIT_DUR`] 而不是 [`MIN_SEGMENT_DUR`]：被收短/后挪的那
/// 一句同样会落进鬼影判据的区间里，余量太小等于把「删句」这件事推给邻居。
pub fn plan_time_edit(
    start: f64,
    end: f64,
    prev: Option<(f64, f64)>,
    next: Option<(f64, f64)>,
    d_start: f64,
    d_end: f64,
) -> TimeEdit {
    let mut new_start = (start + d_start).max(0.0);
    let mut new_end = (end + d_end).max(new_start + MIN_EDIT_DUR);
    let mut prev_end = None;
    let mut next_start = None;

    // 只修用户这次真正碰过的那一侧，避免「改终点却动了前一句」这种莫名连带
    if d_start != 0.0 {
        if let Some((p_start, p_end)) = prev {
            if new_start < p_end {
                // 收短前一句的尾巴，但不能把它压到最短时长以下，也不能反而变长
                let trimmed = new_start.max(p_start + MIN_EDIT_DUR).min(p_end);
                prev_end = Some(trimmed);
                if trimmed > new_start {
                    // 前一句已经让不开：这一句就停在边界上，不再往左
                    new_start = trimmed;
                    new_end = new_end.max(new_start + MIN_EDIT_DUR);
                }
            }
        }
    }

    // 后一句一侧不能只看 `d_end`：把起点往右推时，本句为了维持最短时长会把终点
    // 一起顶过去，同样会压进下一句——这正是用户反复点「起点 +0.5」踩到的坑。
    if let Some((n_start, n_end)) = next {
        if new_end > n_start {
            // 把下一句的头往后挪，同样给它留够最短时长
            let pushed = new_end.min(n_end - MIN_EDIT_DUR).max(n_start);
            if pushed != n_start {
                next_start = Some(pushed);
            }
            if pushed < new_end {
                // 下一句让不开（再挪就没长度了）：本句的终点停在它身上
                new_end = pushed;
            }
        }
    }

    // 兜底：终点被压回之后本句可能已经短于下限，此时只能反过来把起点收回来。
    // 这也是「后一句已经贴脸、退无可退」时的正常收场——拒绝这次微调，而不是
    // 留下一个会被下次载入删掉的短片段。
    if new_end - new_start < MIN_EDIT_DUR {
        new_start = (new_end - MIN_EDIT_DUR).max(0.0);
        if let Some((p_start, p_end)) = prev {
            if new_start < p_end {
                let trimmed = new_start.max(p_start + MIN_EDIT_DUR).min(p_end);
                prev_end = Some(trimmed);
                if trimmed > new_start {
                    new_start = trimmed;
                }
            }
        }
    }

    TimeEdit {
        start: new_start,
        end: new_end,
        prev_end,
        next_start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_text_prefers_polished_then_falls_back() {
        let mut seg = Segment::new(1, 0.0, 1.0, "原始识别文本");
        assert_eq!(
            seg.display_text(),
            "原始识别文本",
            "polished 为空时回退 text"
        );

        seg.polished = "润色后的文本。".to_string();
        assert_eq!(
            seg.display_text(),
            "润色后的文本。",
            "polished 非空时优先返回"
        );

        // 纯空白不算「有润色」，仍回退原文
        seg.polished = "   ".to_string();
        assert_eq!(
            seg.display_text(),
            "原始识别文本",
            "空白 polished 仍回退 text"
        );

        // 导出路径同样吃到润色文本，译文行不受影响
        seg.polished = "润色后的文本。".to_string();
        assert_eq!(seg.export_text(ExportMode::RawOnly), "润色后的文本。");
        seg.translation = Some("translated".to_string());
        assert_eq!(
            seg.export_text(ExportMode::Bilingual),
            "translated\n润色后的文本。"
        );
        assert_eq!(seg.export_text(ExportMode::TranslationOnly), "translated");
    }

    #[test]
    fn test_optimize_segments_removes_ghost_stub() {
        let mut segs = vec![
            Segment::new(1, 10.0, 12.0, "第一句话"),
            Segment::new(2, 12.0, 12.1, "对吧？"),
            Segment::new(3, 12.0, 15.0, "第二句话完整内容"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[0].text, "第一句话");
        assert_eq!(segs[1].index, 2);
        assert_eq!(segs[1].text, "第二句话完整内容");
    }

    #[test]
    fn test_optimize_segments_smooths_short_duration() {
        let mut segs = vec![
            Segment::new(1, 1.0, 1.3, "短句"),
            Segment::new(2, 5.0, 7.0, "后一句"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2);
        assert!(segs[0].end >= 1.8, "短句应延展至至少 0.8 秒");
    }

    #[test]
    fn test_long_segment_splits_at_punctuation() {
        // 模拟用户遇到的 8.25s 大句：应按标点就近均分为两段
        let mut segs = vec![Segment::new(
            1,
            24.070,
            32.320,
            "所以说我们这个专题就总结出来,就帮助大家,就遇到这种题目,咱们至少呢,能够有一个思路,对吧,至少能知道从哪个地方入手。",
        )];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2, "8.25s 大句应拆为两段");
        for seg in &segs {
            assert!(seg.duration() <= 6.0 + 1e-9, "拆分后单段不得超过 6 秒");
        }
        // 时间必须无缝衔接且单调
        assert!((segs[0].end - segs[1].start).abs() < 1e-9);
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2);
        // 文本拼接应还原原句（无丢字）
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("所以说我们这个专题就总结出来"));
        assert!(joined.contains("至少能知道从哪个地方入手"));
    }

    #[test]
    fn test_very_long_segment_splits_recursively() {
        let text = "第一点我们来看这个概念的定义,它在课本当中写得非常清楚,然后第二点我们来看它的几何意义,其实就是面积的表达,然后第三点我们来看例题,通过例题巩固一下,最后再总结一下易错点。";
        let mut segs = vec![Segment::new(1, 10.0, 30.0, text)];
        optimize_segments(&mut segs);
        assert!(segs.len() >= 3, "20s 大句应递归拆成至少三段");
        for seg in &segs {
            assert!(seg.duration() <= 6.0 + 1e-9);
        }
        // 全程时间单调不重叠
        for w in segs.windows(2) {
            assert!(w[0].end <= w[1].start + 1e-9);
        }
    }

    /// 长句拆分时译文/润色必须跟着切点走：右半段不能出现「没有译文」「没有润色」，
    /// 否则双语导出时整句译文只挂在左行（右行只剩原文），
    /// 展示/导出时右半段又会回落成未标点、未清理的原始识别文本。
    #[test]
    fn test_split_keeps_translation_and_polished_on_both_halves() {
        let mut seg = Segment::new(1, 0.0, 8.0, "前面这半句讲的是背景，后面这半句讲的是结论。");
        seg.translation = Some(
            "the first half gives the background and the second half gives the conclusion"
                .to_string(),
        );
        // 润色文本刻意不含标点：按比例硬切也必须两半都有，不能整段留在左边
        seg.polished = "前面这半句讲的是背景后面这半句讲的是结论".to_string();
        let mut segs = vec![seg];
        optimize_segments(&mut segs);

        assert_eq!(segs.len(), 2, "8 秒大句应被拆成两段");
        for s in &segs {
            assert!(
                s.translation
                    .as_deref()
                    .map(|t| !t.trim().is_empty())
                    .unwrap_or(false),
                "右半段丢了译文：{:?}",
                s
            );
            assert!(!s.polished.trim().is_empty(), "右半段丢了润色：{:?}", s);
        }
        let right = &segs[1];
        let exported = right.export_text(ExportMode::Bilingual);
        assert!(
            exported.contains('\n') && exported.contains(right.translation.as_deref().unwrap()),
            "双语导出右行应带上译文行，而不是回落成纯原文：{exported:?}"
        );
    }

    #[test]
    fn optimize_segments_is_idempotent_on_overlapping_input() {
        // 两条几乎同时开始、又互相重叠的片段：第 3 步消重叠会把前一条的尾部收到
        // `start + 0.1`，若不在此次就按鬼影判据删掉，下一次载入（`AppState::load_task_with`
        // 会再跑一遍 `optimize_segments`）数量就会变少，且那条碎条的文本无从找回。
        let mut segs = vec![
            Segment::new(1, 0.0, 3.0, "第一句很长足够长的一句话"),
            Segment::new(2, 0.05, 3.0, "第二句"),
        ];
        optimize_segments(&mut segs);
        let first: Vec<(f64, f64, String)> = segs
            .iter()
            .map(|s| (s.start, s.end, s.text.clone()))
            .collect();

        let mut again = segs.clone();
        optimize_segments(&mut again);
        let second: Vec<(f64, f64, String)> = again
            .iter()
            .map(|s| (s.start, s.end, s.text.clone()))
            .collect();

        assert_eq!(
            first, second,
            "optimize_segments 对重叠输入不幂等：同一工程两次载入结果不同"
        );
    }

    /// 消重叠压出来的 0.1s 碎条必须**当次**就清掉，而不是留到下次载入。
    /// 修前：一次跑完剩两条 `[(31.0,31.1),(31.05,34.0)]`，再跑一遍只剩一条，
    /// 而且被删那条的文本「第一句很长足够长的一句话」直接消失。
    #[test]
    fn overlap_trim_does_not_leave_a_ghost_stub() {
        let mut segs = vec![
            Segment::new(1, 31.0, 34.0, "第一句很长足够长的一句话"),
            Segment::new(2, 31.05, 34.0, "第二句"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(
            segs.len(),
            1,
            "被消重叠压成 0.1s、又与后句起点只差 50ms 的碎条应本次剔除: {segs:?}"
        );
        assert_eq!(segs[0].text, "第二句");
        // 一次跑完即稳定：再跑一遍数量与内容都不变
        let mut again = segs.clone();
        optimize_segments(&mut again);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].text, "第二句");
    }

    /// 幂等性 + 不变量：对若干「重叠 / 等起点 / 三连叠」的恶意输入，
    /// 一次性跑两遍必须完全一致，且结果里不存在交叉区间，也不存在会被下次
    /// 载入当鬼影删掉的短碎片。
    #[test]
    fn optimize_segments_is_idempotent_and_keeps_invariants() {
        let cases: Vec<Vec<Segment>> = vec![
            // 等起点、后一条更长
            vec![
                Segment::new(1, 0.0, 2.0, "甲句内容比较长一些"),
                Segment::new(2, 0.0, 5.0, "乙句内容也不短"),
            ],
            // 前一条被压成 0.1s 的碎条
            vec![
                Segment::new(1, 10.0, 13.0, "第一句很长足够长的一句话"),
                Segment::new(2, 10.08, 13.0, "第二句"),
            ],
            // 三条几乎同起点，制造级联剔除
            vec![
                Segment::new(1, 5.0, 5.5, "短一"),
                Segment::new(2, 5.06, 5.6, "短二"),
                Segment::new(3, 5.07, 9.0, "后面这句子比较长一些"),
            ],
            // 未排序的输入
            vec![
                Segment::new(1, 20.0, 23.0, "后面的句子"),
                Segment::new(2, 20.05, 23.0, "前面的句子"),
            ],
        ];

        for (case_no, case) in cases.into_iter().enumerate() {
            let mut once = case.clone();
            optimize_segments(&mut once);
            let mut twice = once.clone();
            optimize_segments(&mut twice);

            let snap = |segs: &[Segment]| -> Vec<(f64, f64, String)> {
                segs.iter()
                    .map(|s| (s.start, s.end, s.text.clone()))
                    .collect()
            };
            assert_eq!(
                snap(&once),
                snap(&twice),
                "case {case_no} 不幂等: {:?} vs {:?}",
                snap(&once),
                snap(&twice)
            );
            for w in once.windows(2) {
                assert!(
                    w[0].end <= w[1].start + 1e-9,
                    "case {case_no} 结果仍存在交叉区间: {w:?}"
                );
            }
            for i in 0..once.len() {
                assert!(
                    !should_drop_segment(&once[i], once.get(i + 1)),
                    "case {case_no} 结果里仍有会被下次载入剔除的片段: {:?}",
                    once[i]
                );
            }
        }
    }

    #[test]
    fn test_normal_segments_untouched_by_splitter() {
        let mut segs = vec![
            Segment::new(1, 1.0, 4.0, "这是一句正常长度的话,有标点也没关系。"),
            Segment::new(2, 4.0, 5.5, "短句也没事。"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2, "正常句长不应被拆分");
        assert_eq!(segs[0].text, "这是一句正常长度的话,有标点也没关系。");
    }

    #[test]
    fn test_no_punctuation_long_sentence_stays_intact() {
        let text = "这一整句话完全没有出现任何可以作为切分点的标点符号所以保留原样不强行切断";
        let mut segs = vec![Segment::new(1, 0.0, 8.0, text)];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 1, "无标点可切时不强行拆分");
        assert_eq!(segs[0].text, text);
    }

    /// 流式预览入口（`split_long_segments`）对单条超长片段必须与 `optimize_segments`
    /// 内的拆分结果一致：whisper.cpp 的 VAD 在密集讲话下会把 30s+ 整段吐成一条，
    /// 预览若不拆，界面上就是「一句几秒钟说不完的话」。
    #[test]
    fn split_long_segments_splits_single_overlong_segment() {
        let text = "那么第二个我们再来看一下,就是PA加B小于等于PA加B加C,这个是显然的,因为B加C包含了B,对不对,所以概率就更大,那么我们再来看第三个,就是PAB大于等于PABC。";
        let seg = Segment::new(1, 1160.14, 1192.31, text);
        assert!(seg.duration() > MAX_SEGMENT_DUR, "前提：该段确实超长");

        let out = split_long_segments(vec![seg]);
        assert!(out.len() >= 2, "30s 级长段应被拆成多条，实际 {}", out.len());
        for s in &out {
            // 契约：能拆则拆到 6 秒以内；中段 50% 附近无可用标点时按设计保留原样
            // （`split_segment_recursive` 的「无合理标点切点时保留原句」分支）。
            if s.duration() > MAX_SEGMENT_DUR + 1e-9 {
                assert!(
                    split_index_at_ratio(&s.text, 0.5).is_none(),
                    "可拆却未拆到 6 秒内: {:?}",
                    (s.start, s.end, s.duration(), &s.text)
                );
            }
        }
        // 时间必须首尾相接、单调不重叠，且严格落在原段区间内
        assert!((out[0].start - 1160.14).abs() < 1e-9);
        assert!((out.last().unwrap().end - 1192.31).abs() < 1e-9);
        for w in out.windows(2) {
            assert!((w[0].end - w[1].start).abs() < 1e-9, "拆分处必须无缝衔接");
        }
        // 无丢字
        let joined: String = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, text);
    }

    /// 终点往右加：越界时把下一句的头往后挪。
    #[test]
    fn test_plan_time_edit_pushes_next_start() {
        let edit = plan_time_edit(1.0, 2.0, None, Some((2.0, 4.0)), 0.0, 0.5);
        assert_eq!(edit.start, 1.0, "只动终点不该改起点");
        assert_eq!(edit.end, 2.5);
        assert_eq!(edit.next_start, Some(2.5), "下一句的头应被后挪到本句终点");
        assert_eq!(edit.prev_end, None, "改终点不该牵连前一句");
    }

    /// 起点往左加：越界时收短前一句的尾巴。
    #[test]
    fn test_plan_time_edit_trims_prev_end() {
        let edit = plan_time_edit(3.0, 4.0, Some((1.0, 3.0)), None, -0.5, 0.0);
        assert_eq!(edit.start, 2.5);
        assert_eq!(edit.prev_end, Some(2.5), "前一句尾巴应收短到本句新起点");
        assert_eq!(edit.next_start, None);
    }

    /// 反复点「起点 +0.5」：起点右移会把终点顶着一起右移（要维持最短时长），
    /// 因此同样可能压进下一句。把上一个 fix 前的行为（只认 `d_end`）留在这里当回归：
    /// 旧实现会产出交叉区间，且把本句压成 0.1s 的鬼影候选，下次载入被删。
    #[test]
    fn test_plan_time_edit_repeated_start_push_stays_valid() {
        let mut segs = vec![(0.0, 2.0), (2.0, 4.0), (4.0, 6.0)];
        for _ in 0..8 {
            let (s, e) = segs[0];
            let edit = plan_time_edit(s, e, None, Some(segs[1]), 0.5, 0.0);
            segs[0] = (edit.start, edit.end);
            if let Some(ns) = edit.next_start {
                segs[1].0 = ns;
            }
            for w in segs.windows(2) {
                assert!(w[0].1 <= w[1].0 + 1e-9, "时间轴出现交叉：{w:?}");
            }
            for seg in &segs {
                assert!(
                    seg.1 - seg.0 >= MIN_EDIT_DUR - 1e-9,
                    "出现过短片段，下次载入会被当鬼影删掉：{seg:?}"
                );
            }
        }
    }
    // ─────────── 翻译相关的取值与判定 ───────────

    /// 翻译源文本必须用**标点恢复后的文本**，而不是裸识别结果。
    /// 标点恢复是给模型补断句线索的关键，用无标点的 `text` 去翻译会丢线索。
    #[test]
    fn translate_source_prefers_punctuated_text() {
        let mut seg = Segment::new(1, 0.0, 1.0, "你好世界这是一个测试");
        assert_eq!(seg.translate_source(), "你好世界这是一个测试");

        seg.polished = "你好，世界。这是一个测试。".to_string();
        assert_eq!(
            seg.translate_source(),
            "你好，世界。这是一个测试。",
            "应优先取标点恢复后的文本"
        );
    }

    /// 源文本里的换行必须压成空格：批量协议是「一行一序号」，
    /// 保留换行会让一条字幕被拆成两行、错位到相邻序号上。
    #[test]
    fn translate_source_flattens_newlines() {
        let seg = Segment::new(1, 0.0, 1.0, "第一行\n第二行\r\n第三行");
        let src = seg.translate_source();
        assert!(!src.contains('\n'), "不应残留换行: {src:?}");
        assert!(!src.contains('\r'), "不应残留回车: {src:?}");
        assert_eq!(src, "第一行 第二行  第三行");
    }

    /// 只有「译文非空 **且** 语言匹配」才算已完成。
    /// 这是切换目标语言后必须整篇重译的判定基础。
    #[test]
    fn translation_matches_requires_same_language() {
        let mut seg = Segment::new(1, 0.0, 1.0, "你好");
        // 没有译文
        assert!(!seg.translation_matches("English"));
        // 有译文但没记语言（旧数据）：不算「已是目标语言」，必须重译
        seg.translation = Some("Hello".to_string());
        assert!(
            !seg.translation_matches("English"),
            "旧数据缺 translation_lang 时应保守重译，而不是当作已完成"
        );
        // 语言记上了
        seg.translation_lang = Some("English".to_string());
        assert!(seg.translation_matches("English"));
        // 换目标语言 → 不再匹配（旧英文译文不能充当日语译文）
        assert!(
            !seg.translation_matches("日本語"),
            "切换到新语言后旧译文必须判为未完成，否则整片会静默保留旧语言"
        );
        // 空白译文不算完成
        seg.translation = Some("   ".to_string());
        assert!(!seg.translation_matches("English"));
    }
    /// 术语表合规检查：原文含术语但译文未含对应译法 → 标记；已遵守或未带译文 → 不标记。
    #[test]
    fn glossary_violations_flags_only_unfollowed_terms() {
        let entries = vec![
            ("Transformer".to_string(), "变换器".to_string()),
            ("GPT".to_string(), "生成式预训练模型".to_string()),
        ];

        let mut good = Segment::new(1, 0.0, 1.0, "这里讲 Transformer 架构");
        good.translation = Some("This covers the 变换器 architecture.".to_string());

        let mut bad = Segment::new(2, 1.0, 2.0, "GPT 是核心");
        bad.translation = Some("GPT is the core.".to_string()); // 未用「生成式预训练模型」

        let mut no_trans = Segment::new(3, 2.0, 3.0, "Transformer 很好");
        no_trans.translation = None; // 无译文不检查

        let mut unrelated = Segment::new(4, 3.0, 4.0, "今天天气不错");
        unrelated.translation = Some("Nice weather today.".to_string());

        let segs = vec![good, bad, no_trans, unrelated];
        let bad_idx = glossary_violations(&segs, &entries);
        assert_eq!(
            bad_idx,
            vec![2],
            "只应标记未按术语译出的第 2 句: {bad_idx:?}"
        );
    }

    /// 空术语表 → 零标记；大小写不敏感。
    #[test]
    fn glossary_violations_empty_and_case_insensitive() {
        let mut seg = Segment::new(1, 0.0, 1.0, "GPT 模型");
        seg.translation = Some("The 生成式预训练模型 works.".to_string());
        assert!(glossary_violations(&[seg.clone()], &[]).is_empty());
        let mut lower = Segment::new(2, 0.0, 1.0, "gpt 模型");
        lower.translation = Some("The 生成式预训练模型 works.".to_string());
        assert!(glossary_violations(
            &[lower],
            &[("GPT".to_string(), "生成式预训练模型".to_string())]
        )
        .is_empty());
    }

    /// 空串 / 纯空白译文不算「有译文」：双语导出必须回落为单行原文，不能多出一行空白。
    #[test]
    fn has_translation_ignores_blank_and_export_falls_back() {
        let mut seg = Segment::new(1, 0.0, 1.0, "你好");
        assert!(!seg.has_translation(), "无译文时不应判为有");

        seg.translation = Some(String::new());
        assert!(!seg.has_translation(), "空串译文不算有译文");

        seg.translation = Some("   ".to_string());
        assert!(!seg.has_translation(), "纯空白译文不算有译文");
        // 双语导出应回落成单行原文（用 display_text），不拼出空白第二行
        assert_eq!(seg.export_text(ExportMode::Bilingual), "你好");
        assert_eq!(seg.export_text(ExportMode::TranslationOnly), "你好");

        seg.translation = Some("Hello".to_string());
        assert!(seg.has_translation());
        assert_eq!(seg.export_text(ExportMode::Bilingual), "Hello\n你好");
        assert_eq!(seg.export_text(ExportMode::TranslationOnly), "Hello");
    }

    /// 拆分片段时译文与**目标语言**都要跟随，否则拆完一半会变成「无语言标记」，
    /// 下次翻译判定会把它当未完成而重复请求。
    #[test]
    fn split_keeps_translation_lang_on_both_halves() {
        // 注意：拆分切点取自 `text`（不是 `polished`），且必须命中标点才拆
        // （见 `split_index_at_ratio` 与 `test_no_punctuation_long_sentence_stays_intact`），
        // 所以这里给 `text` 本身带标点，才能真的走到「拆成多段」的分支。
        let mut seg = Segment::new(1, 0.0, 10.0, "前面这半句讲的是背景，后面这半句讲的是结论。");
        seg.polished = "前面这半句讲的是背景，后面这半句讲的是结论。".to_string();
        seg.translation =
            Some("The first half is background, the second half is the conclusion.".to_string());
        seg.translation_lang = Some("English".to_string());

        let out = split_long_segments(vec![seg]);
        assert!(out.len() > 1, "应被拆成多段");
        for piece in &out {
            assert_eq!(
                piece.translation_lang.as_deref(),
                Some("English"),
                "拆出的片段必须保留目标语言标记"
            );
        }
    }
    /// 语言码 → 可读名；未知码原样返回（不 panic、不丢信息）。
    #[test]
    fn language_name_maps_known_and_passes_unknown() {
        assert_eq!(language_name("zh"), "中文");
        assert_eq!(language_name("ZH-CN"), "中文");
        assert_eq!(language_name("en"), "英语");
        assert_eq!(language_name("ja"), "日语");
        // 未知码原样返回（去空白），不 panic
        assert_eq!(language_name("  xx  "), "xx");
    }

    /// 众数取整批最常出现的语言；全空返回 None。
    #[test]
    fn dominant_language_takes_mode() {
        let mut a = Segment::new(1, 0.0, 1.0, "一");
        a.language = Some("ja".to_string());
        let mut b = Segment::new(2, 1.0, 2.0, "二");
        b.language = Some("ja".to_string());
        let mut c = Segment::new(3, 2.0, 3.0, "三");
        c.language = Some("en".to_string());
        let segs = [a, b, c];
        assert_eq!(dominant_language(segs.iter()).as_deref(), Some("ja"));

        // 全空 → None
        let none = [Segment::new(1, 0.0, 1.0, "x")];
        assert_eq!(dominant_language(none.iter()), None);

        // 空串语言码同样忽略
        let mut blank = Segment::new(1, 0.0, 1.0, "x");
        blank.language = Some("   ".to_string());
        let blank_segs = [blank];
        assert_eq!(dominant_language(blank_segs.iter()), None);
    }

    /// 剪辑工程导出用的单行文本：三态导出模式 + 内部换行压平。
    /// 工程文件里「一条轨道项 = 一行文字」，绝不能出现换行。
    #[test]
    fn project_export_text_single_line_by_mode() {
        let mut seg = Segment::new(1, 0.0, 1.0, "第 一 行\n第 二 行");
        // 无译文：三种模式都退回原文（且压平换行）
        assert_eq!(
            seg.project_export_text(ExportMode::RawOnly),
            "第 一 行 第 二 行"
        );
        assert_eq!(
            seg.project_export_text(ExportMode::TranslationOnly),
            "第 一 行 第 二 行"
        );
        assert_eq!(
            seg.project_export_text(ExportMode::Bilingual),
            "第 一 行 第 二 行"
        );

        seg.translation = Some("first\nsecond".to_string());
        assert_eq!(
            seg.project_export_text(ExportMode::RawOnly),
            "第 一 行 第 二 行"
        );
        assert_eq!(
            seg.project_export_text(ExportMode::TranslationOnly),
            "first second"
        );
        // 双语 = 译文␠␠原文（与 export_text 的 Bilingual 同序），压成一行
        assert_eq!(
            seg.project_export_text(ExportMode::Bilingual),
            "first second  第 一 行 第 二 行"
        );

        // 空白译文不算译文
        seg.translation = Some("   \n  ".to_string());
        assert_eq!(
            seg.project_export_text(ExportMode::Bilingual),
            "第 一 行 第 二 行"
        );
    }

    // ==================== 质检报告 (P1-9) ====================

    /// 有 / 无置信度混合：`None` 只进 `confidence_missing`，**绝不**进低置信。
    #[test]
    fn quality_report_none_confidence_is_never_low_confidence() {
        let mut low = Segment::new(1, 0.0, 1.0, "低置信的一句");
        low.confidence = Some(-0.9);
        let mut ok = Segment::new(2, 1.0, 2.0, "正常的一句话");
        ok.confidence = Some(-0.05);
        // SenseVoice：confidence 恒为 None
        let none = Segment::new(3, 2.0, 3.0, "没有置信度的一句");

        let r = quality_report(
            &[low, ok, none],
            &[],
            DEFAULT_LOW_CONFIDENCE_THRESHOLD,
            false,
        );
        assert_eq!(r.low_confidence, vec![1], "只有 -0.9 那句低于阈值");
        assert_eq!(r.confidence_scored, 2);
        assert_eq!(r.confidence_missing, 1);
        assert!(!r.confidence_unavailable(), "并不是全部缺置信度");
        assert_eq!(r.total_issues(), 1);
    }

    /// 阈值边界：**等于**阈值不算低置信（与 `plan_rescue_spans` 的 `c < threshold` 一致）。
    #[test]
    fn quality_report_threshold_is_strict() {
        let mut eq = Segment::new(1, 0.0, 1.0, "正好等于阈值");
        eq.confidence = Some(-0.35);
        let mut below = Segment::new(2, 1.0, 2.0, "略低于阈值");
        below.confidence = Some(-0.350_001);
        let mut above = Segment::new(3, 2.0, 3.0, "略高于阈值");
        above.confidence = Some(-0.349_999);

        let r = quality_report(&[eq, below, above], &[], -0.35, false);
        assert_eq!(r.low_confidence, vec![2], "严格小于才算");
        assert_eq!(r.confidence_scored, 3);
        assert_eq!(r.total_issues(), 1);
    }

    /// 空输入 → 判为通过，且**不**谎报「置信度不可用」。
    #[test]
    fn quality_report_empty_input_is_clean() {
        let r = quality_report(&[], &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert!(r.is_clean());
        assert_eq!(r.total_issues(), 0);
        assert_eq!(r.first_issue(), None);
        assert_eq!(r.confidence_scored, 0);
        assert_eq!(r.confidence_missing, 0);
        assert!(!r.confidence_unavailable(), "空输入没有能力缺口可言");
    }

    /// 同一句命中多条：分类列表**不去重**（保留「这句有两个毛病」），
    /// 只有 `all_issues`（一键定位用）合并去重。
    #[test]
    fn quality_report_multi_hit_per_category_kept_but_all_issues_deduped() {
        // 单字符 → 空/超短；-1.2 → 低置信；译文里没有「变换器」→ 术语违规
        let mut seg = Segment::new(7, 0.0, 1.0, "T");
        seg.confidence = Some(-1.2);
        seg.translation = Some("no term here".to_string());
        let entries = vec![("T".to_string(), "变换器".to_string())];

        let r = quality_report(&[seg], &entries, DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert_eq!(r.low_confidence, vec![7]);
        assert_eq!(r.glossary_violations, vec![7]);
        assert_eq!(r.empty_or_short, vec![7]);
        assert!(r.untranslated.is_empty(), "有译文就不算未翻译");
        assert_eq!(r.total_issues(), 3, "同一句命中三条，按条目计数");
        assert_eq!(r.all_issues(), vec![7], "一键定位的合并列表去重");
        assert_eq!(r.first_issue(), Some(7));
    }

    /// 多个判据混排时 `all_issues` 升序去重。
    #[test]
    fn quality_report_all_issues_sorted_and_deduped() {
        let mut a = Segment::new(5, 0.0, 1.0, "低置信");
        a.confidence = Some(-2.0);
        let b = Segment::new(2, 1.0, 2.0, "嗯");
        let mut c = Segment::new(5, 2.0, 3.0, "低置信且超短的 5 号句");
        c.confidence = Some(-2.0);
        let mut d = Segment::new(9, 3.0, 4.0, "嗯");
        d.confidence = Some(-2.0);
        let r = quality_report(&[a, b, c, d], &[], -0.35, false);
        assert_eq!(r.all_issues(), vec![2, 5, 9]);
        assert_eq!(r.first_issue(), Some(2));
    }

    /// 「未翻译」由调用方开关决定；空句不重复计入未翻译。
    #[test]
    fn quality_report_untranslated_is_behind_caller_switch() {
        let raw = Segment::new(1, 0.0, 1.0, "有原文没有译文");
        let off = quality_report(
            std::slice::from_ref(&raw),
            &[],
            DEFAULT_LOW_CONFIDENCE_THRESHOLD,
            false,
        );
        assert!(off.untranslated.is_empty(), "整篇未翻译时不算缺陷");
        let on = quality_report(&[raw], &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert_eq!(on.untranslated, vec![1]);

        // 纯空白译文等同没有译文；但它同时是空句，不该在未翻译里重复出现
        let mut blank = Segment::new(2, 1.0, 2.0, "   ");
        blank.translation = Some("  ".to_string());
        let r = quality_report(&[blank], &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert!(r.untranslated.is_empty(), "空句不进未翻译清单");
        assert_eq!(r.empty_or_short, vec![2]);
    }

    /// 空 / 超短句判据：空白与单字符命中，两字符与正常句不命中。
    /// 同时验证「整批都没有置信度」→ 低置信判据标记为不可用。
    #[test]
    fn quality_report_flags_empty_and_short_only() {
        let segs = vec![
            Segment::new(1, 0.0, 1.0, "   "),
            Segment::new(2, 1.0, 2.0, "嗯"),
            Segment::new(3, 2.0, 3.0, "好吧"),
            Segment::new(4, 3.0, 4.0, "这是一句正常的话"),
        ];
        let r = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, false);
        assert_eq!(r.empty_or_short, vec![1, 2]);
        assert_eq!(r.confidence_missing, 4);
        assert!(r.confidence_unavailable(), "全缺置信度 → 低置信判据不可用");
        assert!(r.low_confidence.is_empty());
    }

    /// 超短判据看的是 `display_text()`（润色优先），不是裸 `text`。
    #[test]
    fn quality_report_short_check_uses_display_text() {
        let mut seg = Segment::new(1, 0.0, 1.0, "嗯");
        seg.polished = "嗯，这里其实有一整句话。".to_string();
        let r = quality_report(&[seg], &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, false);
        assert!(r.empty_or_short.is_empty(), "润色后不短了");
    }

    /// 术语违规与 `glossary_violations` 逐项一致（同源，不另起判据）；空术语表零结果。
    #[test]
    fn quality_report_reuses_glossary_violations() {
        let entries = vec![("GPT".to_string(), "生成式预训练模型".to_string())];
        let mut good = Segment::new(1, 0.0, 1.0, "GPT 模型");
        good.translation = Some("生成式预训练模型 works".to_string());
        let mut bad = Segment::new(2, 1.0, 2.0, "GPT 是核心");
        bad.translation = Some("GPT is the core.".to_string());
        let segs = vec![good, bad];

        let r = quality_report(&segs, &entries, DEFAULT_LOW_CONFIDENCE_THRESHOLD, false);
        assert_eq!(r.glossary_violations, glossary_violations(&segs, &entries));
        assert_eq!(r.glossary_violations, vec![2]);

        let empty = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, false);
        assert!(empty.glossary_violations.is_empty(), "空术语表不产生违规");
    }
}
