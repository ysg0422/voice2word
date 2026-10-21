//! 字幕片段数据结构

use serde::{Deserialize, Serialize};

/// 容忍 `null` 的反序列化：把 `null` 当成「用默认值」，而不是让整份 JSON 解析失败。
///
/// # 为什么需要它
///
/// 这些字段的历史形态里出现过 `null`（例如 `words` 早年很可能就是
/// `Option<Vec<..>>`，外部工具照着自己的模型写出的 JSON 里也常见 `"words": null`）。
/// 而字段现在是非 Option 的 `Vec` / `String`，默认 serde 遇到 `null` 会**硬失败**——
/// 后果不是「这个字段是空的」，而是 **整条 `segments_json` 被判为坏数据**：
/// 用户看到的是「这个工程的字幕无法装载」，一整份工程就此打不开。
///
/// 只多接受 `null` 这一种写法：类型不对（数字、字符串）仍然照常报错，
/// 不会把真正的脏数据当成默认值吞掉。
fn null_is_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub index: usize,
    /// 起始时间 (秒)
    pub start: f64,
    /// 结束时间 (秒)
    pub end: f64,
    /// 原始转写识别文本 (原声识别内容，直接声音是啥就是啥)
    #[serde(default, deserialize_with = "null_is_default")]
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
    /// 多语言译文包：`语言名 -> 译文`。
    ///
    /// `translation` / `translation_lang` 仍是「**当前展示 / 导出用的那一份**」，
    /// 预览、双语导出、统计口径一行都不用改；本字段只是把**译过的每种语言各留一份**，
    /// 供「字幕语言包」切换。此前换目标语言重译会把旧语言的译文直接覆盖掉，
    /// 用户再也切不回去，也看不出这部片子到底译过哪几种语言。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub translations: std::collections::BTreeMap<String, String>,
    /// LLM 润色文本 (保留字段兼容)
    #[serde(default, deserialize_with = "null_is_default")]
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
    /// 说话人自定义名称（如 "张三"、"主讲人"）。设置后 UI 与导出优先展示该名称。
    #[serde(default)]
    pub speaker_name: Option<String>,
    /// 词级时间戳（秒）。空表示该段没有词级信息（旧记录 / 非 Whisper 引擎）。
    ///
    /// 容忍 `null`（老库与外部工具写出过 `"words": null`），见 [`null_is_default`]。
    #[serde(default, deserialize_with = "null_is_default")]
    pub words: Vec<WordTiming>,
}

/// 词级时间戳（秒，来自 Whisper token 级 offsets）。
///
/// 独立成结构而不是塞进 text、也不另开一张表：逐词高亮、卡拉OK 式滚动、按词重排
/// 断句都要按「段 → 词」的顺序取用，只有随 `Segment` 一起序列化才能读写无损。
/// 没有词级信息的旧记录 / 非 Whisper 引擎保持空表，不影响既有行为。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordTiming {
    /// 词/子词文本（沿用 whisper.cpp 原始 token 文本，可能带前导空格）
    pub word: String,
    /// 起始时间 (秒)
    pub start: f64,
    /// 结束时间 (秒)
    pub end: f64,
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
            translations: std::collections::BTreeMap::new(),
            polished: String::new(),
            language: None,
            confidence: None,
            speaker: None,
            speaker_name: None,
            words: Vec::new(),
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
            (Some(text), Some(lang)) => {
                if text.trim().is_empty() || lang != target_lang {
                    return false;
                }
                // 防脏数据兜底：如果目标语言是中文（简体/繁体），但译文中包含日文假名或韩文，
                // 绝不可能是有效的中文译文（必定是此前短路 Bug 污染或复制了外语原文），必须判为未匹配。
                if (target_lang.contains('中') || target_lang.starts_with("zh"))
                    && contains_kana_or_hangul(text)
                {
                    return false;
                }
                true
            }
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

    /// 取某个语言包里的译文（空串 / 纯空白视为没有这一份）。
    pub fn translation_pack(&self, lang: &str) -> Option<&str> {
        self.translations
            .get(lang)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// 记入一份语言包。**只存不切换**：调用方自己决定要不要把它设为当前展示那份
    /// （翻译收尾时通常就是「设为当前」）。
    pub fn set_translation_pack(&mut self, lang: &str, text: &str) {
        let lang = lang.trim();
        if lang.is_empty() || text.trim().is_empty() {
            return;
        }
        self.translations.insert(lang.to_string(), text.to_string());
    }

    /// 把某个语言包切换成「当前展示 / 导出用的那一份」。
    ///
    /// 预览、双语导出、翻译统计全都只看 `translation` + `translation_lang`，
    /// 所以「切换语言包」在这里投影一次，下游一行都不用改。
    pub fn show_translation_pack(&mut self, lang: &str) -> bool {
        match self.translations.get(lang).cloned() {
            Some(text) if !text.trim().is_empty() => {
                self.translation = Some(text);
                self.translation_lang = Some(lang.to_string());
                true
            }
            _ => false,
        }
    }

    /// 回到「只显示原文」：清掉当前展示的译文，但**语言包保留**（还能切回来）。
    pub fn show_source_only(&mut self) {
        self.translation = None;
        self.translation_lang = None;
    }

    /// 该段是否带词级时间戳（words 非空即视为可用）。
    pub fn has_word_timings(&self) -> bool {
        !self.words.is_empty()
    }

    /// 说话人标签（优先使用自定义名称，否则回退至「说话人 N」）
    pub fn speaker_label(&self) -> Option<String> {
        if let Some(ref name) = self.speaker_name {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        // 用饱和加法：序号是外部数据，理论上可到 u32::MAX，`s + 1` 在 debug 构建
        // 会直接 panic（release 回绕成 0），而这里只想渲染一个标签。
        self.speaker
            .map(|s| format!("说话人 {}", s.saturating_add(1)))
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
                // 只有非空译文才拼出第二行；若译文与原文内容一致（如中翻中无变化），自动去重单行显示
                Some(t) if !t.trim().is_empty() => {
                    let raw = self.display_text();
                    if t.trim() == raw.trim() {
                        raw.to_string()
                    } else {
                        format!("{}\n{}", t, raw)
                    }
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
    /// - [`ExportMode::Bilingual`] → `译文  原文`（无译文或译文与原文相同时退回单份原文）。
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
            ExportMode::Bilingual if has_trans => {
                if trans.trim() == raw.trim() {
                    raw
                } else {
                    format!("{trans}  {raw}")
                }
            }
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
/// 该阈值现已接入配置：`PipelineConfig::whisper_low_confidence`
/// （`src/utils/config.rs`，默认 -0.35、用户可调），转写页质检卡、剪辑台高亮与
/// 一键复核都从配置读。此常量保留为「产品默认值」的单一事实来源——
/// `default_low_confidence()` 与它同值，并有单测锁死两者相等。
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

/// 质检问题的分类。与 `QualityReport` 的四个 `Vec<usize>` 一一对应。
///
/// 用枚举而不是让调用方自己挑 `report.low_confidence` / `report.glossary_violations`…：
/// 界面上的「按类翻查」需要把「选中的是哪一类」存进 `MainWindow` 状态
/// （闭包不能进状态，枚举可以），并且需要一个统一的「这一类有哪些句、下一句是谁」入口，
/// 否则四种分类会在 UI 层各写一份导航逻辑，边界行为必然分叉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityCategory {
    LowConfidence,
    GlossaryViolation,
    Untranslated,
    EmptyOrShort,
}

impl QualityCategory {
    /// 全部四类，顺序与界面上的展示顺序一致（低置信 → 术语 → 未翻译 → 碎片句）。
    pub const ALL: [QualityCategory; 4] = [
        QualityCategory::LowConfidence,
        QualityCategory::GlossaryViolation,
        QualityCategory::Untranslated,
        QualityCategory::EmptyOrShort,
    ];

    /// 该类的句序号列表（升序；同一句在同一类里只出现一次）。
    ///
    /// 各 bucket 本身按 `segments` 的顺序 push，而 `segments` 本就按时间升序，
    /// 所以这里**不重新排序**，只做去重保护——重排会让「列表顺序」与「字幕顺序」
    /// 出现两个真相，用户按「下一处」跳的时候会觉得跳来跳去。
    pub fn indices(self, report: &QualityReport) -> Vec<usize> {
        let bucket: &[usize] = match self {
            QualityCategory::LowConfidence => &report.low_confidence,
            QualityCategory::GlossaryViolation => &report.glossary_violations,
            QualityCategory::Untranslated => &report.untranslated,
            QualityCategory::EmptyOrShort => &report.empty_or_short,
        };
        let mut out: Vec<usize> = Vec::with_capacity(bucket.len());
        for &idx in bucket {
            if !out.contains(&idx) {
                out.push(idx);
            }
        }
        out
    }

    /// 界面用的中文短名（与转写页质检卡片的胶囊文案同源，避免两处各写一份）
    pub fn label(self) -> &'static str {
        match self {
            QualityCategory::LowConfidence => "低置信",
            QualityCategory::GlossaryViolation => "术语违规",
            QualityCategory::Untranslated => "未翻译",
            QualityCategory::EmptyOrShort => "空/超短句",
        }
    }
}

/// 在某一类质检问题里找「当前句之后的下一处」；越过末尾则回卷到第一处。
///
/// - `current = None` → 返回该类第一处；
/// - 该类为空 → `None`；
/// - `current` 本身就是最后一处 → 回卷到第一处（与 `MainWindow` 的
///   `quality_next_issue` 对「全部问题」的既有语义一致，用户可以循环翻查）。
///
/// 抽成纯函数是为了可单测：这几个边界（空类、无当前、越过末尾回卷）在界面里
/// 各写一遍必然漏一两个，而它们恰恰是用户最容易碰到的（一路点到最后一处）。
pub fn next_issue_in(current: Option<usize>, indices: &[usize]) -> Option<usize> {
    // 列表约定升序，直接取第一个严格大于当前句的即可；current 为 None 时
    // 视作「-∞」，于是返回第一处。current 本身也是问题时会被 `>` 跳过——
    // 这正是「下一处」该有的行为，不能写 `>=` 否则会原地打转。
    let probe = match current {
        Some(i) => i,
        // 没有当前句：从该类第一处起步；空类自然返回 None。
        None => return indices.first().copied(),
    };
    if let Some(&next) = indices.iter().find(|&&i| i > probe) {
        return Some(next);
    }
    // 越界（当前句在最后一处或之后）：回卷到第一处；空类自然返回 None。
    indices.first().copied()
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
    // 用**有序**表而不是 HashMap：并列众数必须给出确定的结果，否则同一批字幕两次
    // 调用可能拿到不同语言码（HashMap 迭代顺序随机），翻译的源语言提示会来回闪。
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
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
    // 严格大于才替换：并列时保留键最小的那个（BTreeMap 升序迭代），结果与输入顺序无关。
    let mut best: Option<(&str, usize)> = None;
    for (lang, n) in counts {
        let better = match best {
            Some((_, best_n)) => n > best_n,
            None => true,
        };
        if better {
            best = Some((lang, n));
        }
    }
    best.map(|(lang, _)| lang.to_string())
}

/// 按导出模式取文本，并按需带上说话人前缀。
pub fn export_text_for(seg: &Segment, mode: ExportMode, with_speaker: bool) -> String {
    if with_speaker {
        seg.text_with_speaker(mode)
    } else {
        seg.export_text(mode)
    }
}

/// 是否包含日文假名（平假名 / 片假名 / 片假名音标扩展）
pub fn contains_japanese_kana(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c as u32, 0x3040..=0x309F | 0x30A0..=0x30FF | 0x31F0..=0x31FF))
}

/// 是否包含韩文（谚文字母 / 谚文音节）
pub fn contains_korean_hangul(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c as u32, 0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F))
}

/// 是否包含假名或韩文（常用于检测目标语言为中文时是否被原样复制或错转）
pub fn contains_kana_or_hangul(s: &str) -> bool {
    contains_japanese_kana(s) || contains_korean_hangul(s)
}

/// 判断当前字幕片段的源文本是否为中文。
///
/// 只有当源语言是中文时，目标语言为中文变体（简体/繁体）才允许使用 zhconv 进行字面确定性转换。
/// 若源文本为日语（含假名）、韩语（含谚文）、英语（拉丁文）等外语，必须交由 LLM / 在线 API 进行真正的跨语言翻译！
pub fn is_source_chinese(segments: &[Segment]) -> bool {
    if segments.is_empty() {
        return false;
    }
    // 1. 如果有标注语言，检查标注语言
    let dominant = dominant_language(segments);
    if let Some(ref lang) = dominant {
        let l = lang.trim().to_ascii_lowercase();
        if l == "ja"
            || l == "en"
            || l == "ko"
            || l == "ru"
            || l == "fr"
            || l == "de"
            || l == "es"
            || l == "it"
        {
            return false;
        }
        if l.starts_with("zh") || l.starts_with("cmn") || l.starts_with("yue") {
            // 即便标了 zh，也要检查是否有日文假名或韩文（防止转写模型把外语误标为中文）
            for seg in segments {
                if contains_kana_or_hangul(&seg.translate_source()) {
                    return false;
                }
            }
            return true;
        }
    }

    // 2. 检查实际文本字符特征
    let mut total_chars = 0usize;
    let mut cjk_chars = 0usize;
    let mut latin_chars = 0usize;

    for seg in segments {
        let text = seg.translate_source();
        if contains_kana_or_hangul(&text) {
            return false;
        }
        for c in text.chars() {
            if c.is_whitespace() || c.is_ascii_punctuation() {
                continue;
            }
            total_chars += 1;
            if matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
            {
                cjk_chars += 1;
            } else if c.is_ascii_alphabetic() {
                latin_chars += 1;
            }
        }
    }

    if total_chars == 0 || cjk_chars == 0 {
        return false;
    }

    // 汉字数量必须多于西文字母，且汉字占比需达到阈值（>= 30%）
    cjk_chars >= 4 && cjk_chars > latin_chars && (cjk_chars as f64 / total_chars as f64) >= 0.3
}

/// 智能优化字幕时间轴与消除鬼影/闪烁：
/// 1. 按起始时间升序排序
/// 2. 剔除无效片段 (duration <= 0.05s) 以及模型幻读鬼影（< 0.25s、与后句几乎同时启动，
///    **且文本只有 1~2 个字符并与后句高度相似**）
/// 3. 消除时间戳倒退与重叠冲突（前句尾部不超出后句头部；优先保留文本更长的那条，
///    万不得已才删文本更短的那条，且绝不把幸存者压到 [`MIN_EDIT_DUR`] 以下）
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

    // 3. 消除时间重叠：优先保留文本更长的句子。
    //
    // 旧实现无条件把前句终点压到 `start + 0.1`（再交给第 5 步当鬼影删掉），
    // 于是一条先开始的 3 秒完整台词会被后一句 1 秒短句"截肢"后整句消失。
    // 这里改成按文本长度裁决：长句保住完整区间，短句让位；短句让不开
    // （让完不足 MIN_EDIT_DUR）时删短句，而不是压长句。
    resolve_overlaps_prefer_longer(&mut filtered);

    // 4. 长句自动拆分：转写初始阶段就把 8 秒级大句按标点切短，句长可控且画面字幕不超屏
    let filtered = split_long_segments(filtered);

    // 5. 复扫：第 3 步消重叠只改边界（且在改的时候已保证不短于 `MIN_EDIT_DUR`），
    // 第 4 步拆分也可能留下短碎片。若留到下一次载入才由第 2 步剔除，用户看到的就是
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

    // 7. 重新编排序号，并把词级时间戳收敛回本段区间。
    //
    // 第 3/6 步只调边界、不重算 words：消重叠会把前句 nd 收短、极短句平滑会把
    // nd 提前，而词时间仍停在更远处。词级时间戳是一等数据结构，必须与段边界自洽，
    // 否则下游做逐词高亮 / 对齐时会读到段外的时刻。这里统一收口。
    for (idx, seg) in filtered.iter_mut().enumerate() {
        seg.index = idx + 1;
        clamp_words_to_segment(seg);
    }

    *segments = filtered;
}

/// 消重叠时的文本长度口径：按字符计、忽略空白（标点算字符，因为「嗯？」比「嗯」
/// 更像一句真实台词，不该因为标点被当成更短的一方优先删掉）。
fn overlap_text_len(seg: &Segment) -> usize {
    seg.display_text()
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
}

/// 按「文本更长者优先」消除相邻重叠，必要时删掉文本更短的那条。
///
/// 与旧的「无条件压前句」相比，这里守两条不变量：
/// * 只要两段都还能保住 [`MIN_EDIT_DUR`]，就都留下（长句保留完整区间，短句让位）；
/// * 短句让不开时删的是**文本更短**的那条，绝不把长句压成 0.1s 的鬼影候选。
///
/// 全程要求输入已按 start 升序（[`optimize_segments`] 第 1 步保证）。被调整/删除后
/// 的邻居关系会重新检查，因此删除短句后暴露出的新重叠也会在同一趟解决。
fn resolve_overlaps_prefer_longer(filtered: &mut Vec<Segment>) {
    // 浮点毛刺不算重叠，也用来容忍「恰好等于下限」的边界。否则 1e-12 秒的接缝
    // 就会走进删除分支，把一条正常台词当成让不开的短句删掉。
    const TIME_EPS: f64 = 1e-9;

    let mut j = 0;
    while j + 1 < filtered.len() {
        if filtered[j].end <= filtered[j + 1].start + TIME_EPS {
            j += 1;
            continue;
        }

        let keep_prev = overlap_text_len(&filtered[j]) >= overlap_text_len(&filtered[j + 1]);
        if keep_prev {
            // 后句文本不更长：让它整体后移到前句终点之后（起点可以变，终点不变）。
            let prev_end = filtered[j].end;
            if filtered[j + 1].end - prev_end >= MIN_EDIT_DUR - TIME_EPS {
                filtered[j + 1].start = prev_end;
                j += 1;
            } else {
                // 让完会短于下限：删文本更短的后句，保住前句完整区间。
                filtered.remove(j + 1);
            }
        } else {
            // 后句文本更长：收前句尾部到后句起点。
            let next_start = filtered[j + 1].start;
            if next_start - filtered[j].start >= MIN_EDIT_DUR - TIME_EPS {
                filtered[j].end = next_start;
                j += 1;
            } else {
                // 前句让完会短于下限：删文本更短的它；原后句留在 j 位继续与下一条比较。
                filtered.remove(j);
            }
        }
    }
}

/// 把 seg.words 收敛到 [seg.start, seg.end]：与段区间无交集的词丢弃，部分越界的裁剪。
///
/// [optimize_segments] 的边界调整不重算词时间，这里统一收口，保证「段 → 词」自洽。
fn clamp_words_to_segment(seg: &mut Segment) {
    if seg.words.is_empty() {
        return;
    }
    let (start, end) = (seg.start, seg.end);
    seg.words.retain_mut(|w| {
        // 整词落在段区间之外（含边界相接）：丢弃，避免出现 start == end 的空词。
        if w.end <= start || w.start >= end {
            return false;
        }
        w.start = w.start.clamp(start, end);
        w.end = w.end.clamp(start, end);
        true
    });
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
    // 同时间段重叠的极短鬼影碎片：例如「对吧？」被模型同时吐成一个 0.1s 碎片和整句。
    //
    // 判据必须**同时**满足时长、起点、文本三条：只按时长+起点会误删真实台词
    // ——先开始的长句在消重叠时被压短后，正好落进这个组合里被整句丢掉。
    // 文本条件要求「≤2 字符且与后句高度相似」，把「嗯」「对」这类同句碎片与
    // 独立的短台词区分开。
    match next {
        Some(n) => {
            seg.end - seg.start < GHOST_MAX_DUR
                && (n.start - seg.start).abs() < GHOST_START_TOL
                && is_ghost_text_of(seg, n)
        }
        None => false,
    }
}

/// 鬼影碎片与后句的文本相似度：只认「1~2 个有效字符且是后句的前缀/同一句」。
///
/// 归一化去掉空白与标点（`对吧？` / `对吧` 视为同一句），但保留字母/汉字/数字。
fn is_ghost_text_of(seg: &Segment, next: &Segment) -> bool {
    let ghost = ghost_text_key(seg.display_text());
    let ghost_chars = ghost.chars().count();
    if ghost_chars == 0 || ghost_chars > GHOST_MAX_CHARS {
        return false;
    }
    let next = ghost_text_key(next.display_text());
    !next.is_empty() && (next == ghost || next.starts_with(&ghost))
}

/// 鬼影判据的文本归一化：只留字母 / 汉字 / 数字，并统一小写。
fn ghost_text_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
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
pub(crate) fn split_following(text: &str, ratio: f64) -> (String, Option<String>) {
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

/// 按切点时间把词级时间戳分成前后两组：词中点落在切点之前的归左，其余归右。
///
/// 拆分长句时若把 words 直接置空，Whisper 拿到的词级 offsets 会在这一步整体丢失，
/// 下游的逐词高亮 / 对齐就再也取不到数据。这里按「词中点相对切点」二分，保证每个词
/// 只属于一侧且两侧各自保持时间单调；跨切点的词按中点归属，随后由
/// [clamp_words_to_segment] 收口到各自子段区间内，不会越界。
fn split_words_at(words: &[WordTiming], split_t: f64) -> (Vec<WordTiming>, Vec<WordTiming>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for w in words {
        if (w.start + w.end) / 2.0 < split_t {
            left.push(w.clone());
        } else {
            right.push(w.clone());
        }
    }
    (left, right)
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

    let (left_words, right_words) = split_words_at(&seg.words, split_t);

    let mut left = Segment {
        index: 0,
        start: seg.start,
        end: split_t,
        text: left_text,
        translation: left_trans,
        translation_lang: seg.translation_lang.clone(),
        translations: Default::default(),
        polished: left_polished,
        language: seg.language.clone(),
        confidence: seg.confidence,
        speaker: seg.speaker,
        speaker_name: seg.speaker_name.clone(),
        words: left_words,
    };
    let mut right = Segment {
        index: 0,
        start: split_t,
        end: seg.end,
        text: right_text,
        translation: right_trans,
        translation_lang: seg.translation_lang.clone(),
        translations: Default::default(),
        polished: right_polished,
        language: seg.language,
        confidence: seg.confidence,
        speaker: seg.speaker,
        speaker_name: seg.speaker_name,
        words: right_words,
    };
    // 收敛到各自子段区间（含边界），与 optimize_segments 的收口口径一致。
    clamp_words_to_segment(&mut left);
    clamp_words_to_segment(&mut right);
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

/// 鬼影判定里的文本长度上限（有效字符数）。
///
/// 只有「嗯」「对」「好的」这类 1~2 字的同句碎片才可能是模型重复吐字；完整台词
/// （哪怕被裁到 0.1s）不该仅凭时长与起点就被删掉。
const GHOST_MAX_CHARS: usize = 2;

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

    // 兜底之后仍可能短于下限（后一句已经贴脸、前一句也退无可退）：把终点顶回下限，
    // 绝不交出「下次载入会被鬼影判据删掉」的短片段——拒绝一次微调，比悄悄丢一句好。
    if new_end - new_start < MIN_EDIT_DUR {
        new_end = new_start + MIN_EDIT_DUR;
    }

    TimeEdit {
        start: new_start,
        end: new_end,
        prev_end,
        next_start,
    }
}

/// 老库 / 外部工具写出的 `"words": null` 必须仍然能装载。
///
/// 回归护栏：字段类型是非 Option 的 `Vec`，serde 默认遇到 `null` 会**整份 JSON 解析失败**——
/// 用户看到的是「这个工程的字幕无法装载」，而不是「只有一个字段是空的」。
/// 同一份工程在旧版本里能打开、升级后打不开，是最难解释的一类回归。
#[cfg(test)]
mod null_tolerance_tests {
    use super::Segment;

    /// `null` 落到 text / polished / words 上都按默认值处理，其余字段照常解析。
    #[test]
    fn null_fields_fall_back_to_defaults() {
        let json = r#"[
            {"index":1,"start":0.0,"end":1.0,"text":null,"polished":null,"words":null},
            {"index":2,"start":1.0,"end":2.0,"text":"正常","words":[]}
        ]"#;
        let segs: Vec<Segment> = serde_json::from_str(json).expect("null 不应导致整份解析失败");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "", "null 的正文按空串");
        assert_eq!(segs[0].polished, "");
        assert!(segs[0].words.is_empty(), "null 的词级时间戳按空表");
        assert_eq!(segs[1].text, "正常");
    }

    /// 只多接受 `null`：类型真不对（数字当文本、字符串当数组）仍然要报错——
    /// 否则会把真正的脏数据当成默认值吞掉。
    #[test]
    fn wrong_types_still_fail() {
        let bad_text = r#"[{"index":1,"start":0.0,"end":1.0,"text":123}]"#;
        assert!(serde_json::from_str::<Vec<Segment>>(bad_text).is_err());
        let bad_words = r#"[{"index":1,"start":0.0,"end":1.0,"text":"a","words":"不是数组"}]"#;
        assert!(serde_json::from_str::<Vec<Segment>>(bad_words).is_err());
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

        // 译文与原文完全相同时（如中翻中无变化），双语模式应自动去重为单行
        seg.translation = Some("润色后的文本。".to_string());
        assert_eq!(
            seg.export_text(ExportMode::Bilingual),
            "润色后的文本。",
            "译文与原文一致时应单行去重，避免叠两行相同文字"
        );
        let proj = seg.project_export_text(ExportMode::Bilingual);
        assert_eq!(proj, "润色后的文本。", "项目导出也应去重为单语单行");
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
        // 两条几乎同时开始、又互相重叠的片段：第 3 步消重叠会保留文本更长的前句、
        // 删掉让不开的短句。删除必须发生在**本次**调用内，否则下一次载入
        // （`AppState::load_task_with` 会再跑一遍 `optimize_segments`）数量还会变，
        // 且那条被删句的文本无从找回。
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

    /// 两条几乎同时开始、文本一长一短时，必须**保留文本更长的那条**。
    ///
    /// 修前：旧实现无条件把前句终点压到 `start + 0.1`，再把这条 0.1s 碎条按
    /// 「时长 <0.25s 且与后句起点差 <0.15s」当鬼影删掉——结果是 3 秒的完整台词
    /// 「第一句很长足够长的一句话」整句消失，只留下 2 个字的「第二句」。这违背直觉：
    /// 重叠时该让位/被删的是信息更少的那条。现在改成优先保留长文本，删除短文本，
    /// 并且一次跑完就稳定（不会下次载入再变）。
    #[test]
    fn overlap_trim_keeps_longer_text_and_drops_shorter() {
        let mut segs = vec![
            Segment::new(1, 31.0, 34.0, "第一句很长足够长的一句话"),
            Segment::new(2, 31.05, 34.0, "第二句"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(
            segs.len(),
            1,
            "重叠且短句让不开时应删文本更短的「第二句」，而不是删长句: {segs:?}"
        );
        assert_eq!(segs[0].text, "第一句很长足够长的一句话");
        // 一次跑完即稳定：再跑一遍数量与内容都不变
        let mut again = segs.clone();
        optimize_segments(&mut again);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].text, "第一句很长足够长的一句话");
    }

    /// F12 回归：前者是 3 秒完整长句、后者是 1 秒短文本，二者重叠时必须保留前者。
    /// 旧行为会把前者裁成 0.1s 再当鬼影删除，用户载入工程时就永久少一句台词。
    #[test]
    fn overlap_keeps_three_second_line_over_one_second_short_line() {
        let mut segs = vec![
            Segment::new(1, 100.0, 103.0, "这是一句完整的三秒台词"),
            Segment::new(2, 100.5, 101.5, "短句"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 1, "短句让不开时应被删除: {segs:?}");
        assert_eq!(segs[0].text, "这是一句完整的三秒台词");
        assert!(
            segs[0].end - segs[0].start >= 2.9,
            "长句的完整时长不能被重叠消解压掉: {segs:?}"
        );
    }

    /// 鬼影判据收紧后的正例：1~2 字、与后句高度相似的碎片才删。
    #[test]
    fn ghost_rule_still_drops_short_similar_fragment() {
        let mut segs = vec![
            Segment::new(1, 10.0, 12.0, "第一句话"),
            Segment::new(2, 12.0, 12.1, "对"),
            Segment::new(3, 12.0, 15.0, "对，我们继续讲第二句话"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2, "1 个字的同句碎片应被当鬼影删掉: {segs:?}");
        assert_eq!(segs[0].text, "第一句话");
        assert_eq!(segs[1].text, "对，我们继续讲第二句话");
    }

    /// 鬼影判据收紧后的反例：时长与起点都像鬼影，但文本是完整的独立台词，不能删。
    #[test]
    fn ghost_rule_keeps_real_short_line_with_unrelated_text() {
        let mut segs = vec![
            Segment::new(1, 10.0, 12.0, "第一句话"),
            // 0.1s、与后句起点只差 0.12s，旧判据会直接删掉；但「现在开始」是四个字，
            // 不是同句碎片，收紧后必须作为独立短句保留。
            Segment::new(2, 12.0, 12.1, "现在开始"),
            Segment::new(3, 12.12, 15.0, "下一句完全不同的话"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 3, "独立短句不能被鬼影判据删掉: {segs:?}");
        assert!(segs.iter().any(|s| s.text == "现在开始"));
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
            // 短句紧贴长句起点（旧实现会把长句压成 0.1s 碎条）
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

    /// 拆分长句时词级时间戳必须随文本一起切分，而不是被清空：逐词高亮 / 对齐依赖它，
    /// 超长句一旦被拆就丢词级数据等于功能整体失效。
    #[test]
    fn split_long_segments_preserves_and_partitions_word_timings() {
        let text = "那么第二个我们再来看一下,就是PA加B小于等于PA加B加C,这个是显然的,因为B加C包含了B,对不对,所以概率就更大,那么我们再来看第三个,就是PAB大于等于PABC。";
        let (start, end) = (1160.14_f64, 1192.31_f64);
        let mut seg = Segment::new(1, start, end, text);
        // 构造 12 个等分词，覆盖整段区间
        let n = 12usize;
        let step = (end - start) / n as f64;
        seg.words = (0..n)
            .map(|i| WordTiming {
                word: format!("w{i}"),
                start: start + step * i as f64,
                end: start + step * (i as f64 + 1.0),
            })
            .collect();

        let out = split_long_segments(vec![seg]);
        assert!(out.len() >= 2, "前提：该段应被拆成多条，实际 {}", out.len());

        // 词总数守恒（不丢、不重复）
        let total: usize = out.iter().map(|s| s.words.len()).sum();
        assert_eq!(total, n, "拆分后词级时间戳总数必须守恒");

        // 每个词都落在所属子段区间内，且自身 start <= end
        for s in &out {
            for w in &s.words {
                assert!(w.start <= w.end, "词时间必须单调: {w:?}");
                assert!(
                    w.start >= s.start - 1e-9 && w.end <= s.end + 1e-9,
                    "词 {w:?} 越出段区间 [{}, {}]",
                    s.start,
                    s.end
                );
            }
        }

        // 顺序保持：展平后的词序与原始一致
        let flat: Vec<&str> = out
            .iter()
            .flat_map(|s| s.words.iter())
            .map(|w| w.word.as_str())
            .collect();
        let expected: Vec<String> = (0..n).map(|i| format!("w{i}")).collect();
        assert_eq!(
            flat,
            expected.iter().map(|x| x.as_str()).collect::<Vec<_>>()
        );
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

    /// 微调的自身不变式：无论邻居多挤，输出区间都不得短于 [`MIN_EDIT_DUR`]——
    /// 短片段下次载入会被鬼影判据删掉，等于丢句。
    #[test]
    fn test_plan_time_edit_never_outputs_degenerate_interval() {
        let edit = plan_time_edit(0.0, 0.05, None, Some((0.1, 0.2)), -3.0, -3.0);
        assert!(
            edit.end - edit.start >= MIN_EDIT_DUR - 1e-9,
            "起点被拉回时留下过短片段: {edit:?}"
        );
        let edit = plan_time_edit(0.0, 0.3, None, Some((0.1, 0.2)), 0.0, 0.0);
        assert!(
            edit.end - edit.start >= MIN_EDIT_DUR - 1e-9,
            "终点被后一句顶回时留下过短片段: {edit:?}"
        );
    }

    /// 并列众数必须有确定结果（HashMap 迭代顺序随机，会让翻译的源语言提示来回闪）。
    #[test]
    fn dominant_language_tie_is_deterministic() {
        let mut zh = Segment::new(1, 0.0, 1.0, "甲");
        zh.language = Some("zh".to_string());
        let mut en = Segment::new(2, 1.0, 2.0, "乙");
        en.language = Some("en".to_string());
        let segs = [zh, en];
        // 各出现一次时取键最小者，且与传入顺序无关。
        assert_eq!(dominant_language(segs.iter()).as_deref(), Some("en"));
        assert_eq!(dominant_language(segs.iter().rev()).as_deref(), Some("en"));
    }

    /// 极端序号（u32::MAX）下渲染说话人标签不得 panic 或回绕成 0。
    #[test]
    fn speaker_label_saturates_instead_of_overflowing() {
        let mut seg = Segment::new(1, 0.0, 1.0, "文本");
        seg.speaker = Some(u32::MAX);
        assert_eq!(
            seg.speaker_label().as_deref(),
            Some("说话人 4294967295"),
            "饱和加法不该回绕"
        );
    }

    #[test]
    fn speaker_label_prefers_custom_speaker_name() {
        let mut seg = Segment::new(1, 0.0, 1.0, "文本");
        seg.speaker = Some(0);
        assert_eq!(seg.speaker_label().as_deref(), Some("说话人 1"));

        seg.speaker_name = Some("张三".to_string());
        assert_eq!(seg.speaker_label().as_deref(), Some("张三"));

        // 空白或纯空格应回退至默认编号
        seg.speaker_name = Some("   ".to_string());
        assert_eq!(seg.speaker_label().as_deref(), Some("说话人 1"));
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

        // 防脏数据：目标语言为简体中文/繁体中文，但译文里含有日文假名或韩文（如历史短路污染），不应判为已完成
        seg.translation = Some("こんにちは".to_string());
        seg.translation_lang = Some("简体中文".to_string());
        assert!(
            !seg.translation_matches("简体中文"),
            "含日文假名的译文即便标了简体中文，也必须判为未完成"
        );
        seg.translation = Some("안녕하세요".to_string());
        assert!(
            !seg.translation_matches("繁体中文"),
            "含韩文字母的译文即便标了繁体中文，也必须判为未完成"
        );
    }

    #[test]
    fn is_source_chinese_accuracy() {
        // 纯中文
        let zh_segs = vec![Segment::new(
            1,
            0.0,
            1.0,
            "今天的天气真好，我们一起去公园散步吧。",
        )];
        assert!(is_source_chinese(&zh_segs));

        // 日文（含假名）
        let ja_segs = vec![Segment::new(
            1,
            0.0,
            1.0,
            "こんにちは、今日はいい天気ですね。",
        )];
        assert!(!is_source_chinese(&ja_segs));

        // 英文（纯拉丁）
        let en_segs = vec![Segment::new(1, 0.0, 1.0, "Hello world, this is a test.")];
        assert!(!is_source_chinese(&en_segs));

        // 韩文
        let ko_segs = vec![Segment::new(1, 0.0, 1.0, "안녕하세요 여러분.")];
        assert!(!is_source_chinese(&ko_segs));

        // 显式标了 ja
        let mut ja_labeled = Segment::new(1, 0.0, 1.0, "東京大学");
        ja_labeled.language = Some("ja".to_string());
        assert!(!is_source_chinese(&[ja_labeled]));

        // 显式标了 zh 且文本是汉字
        let mut zh_labeled = Segment::new(1, 0.0, 1.0, "北京大学");
        zh_labeled.language = Some("zh".to_string());
        assert!(is_source_chinese(&[zh_labeled]));
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

    // ==================== 按类翻查 (质检导航) ====================

    /// 四类各自只认自己的 bucket：低置信 / 术语 / 未翻译 / 超短句互不串味。
    #[test]
    fn quality_category_indices_map_to_matching_bucket() {
        // 1/2/4 都给上译文，好让「未翻译」只剩 3 号一句，四类各自独立可断言。
        let mut low = Segment::new(1, 0.0, 1.0, "低置信的一句");
        low.confidence = Some(-1.0);
        low.translation = Some("低置信的译文".to_string());
        let mut term = Segment::new(2, 1.0, 2.0, "GPT 是核心");
        term.translation = Some("GPT is the core.".to_string());
        let untranslated = Segment::new(3, 2.0, 3.0, "有原文没译文");
        let mut short = Segment::new(4, 3.0, 4.0, "嗯");
        short.translation = Some("嗯".to_string());
        let entries = vec![("GPT".to_string(), "生成式预训练模型".to_string())];

        let r = quality_report(
            &[low, term, untranslated, short],
            &entries,
            DEFAULT_LOW_CONFIDENCE_THRESHOLD,
            true,
        );
        assert_eq!(QualityCategory::LowConfidence.indices(&r), vec![1]);
        assert_eq!(QualityCategory::GlossaryViolation.indices(&r), vec![2]);
        assert_eq!(QualityCategory::Untranslated.indices(&r), vec![3]);
        assert_eq!(QualityCategory::EmptyOrShort.indices(&r), vec![4]);
    }

    /// 同一类里多句时保持升序（与字幕顺序一致），并且同句重复只保留一次。
    #[test]
    fn quality_category_indices_are_ascending_and_deduped() {
        let mut a = Segment::new(3, 0.0, 1.0, "低置信甲");
        a.confidence = Some(-2.0);
        let mut b = Segment::new(7, 1.0, 2.0, "低置信乙");
        b.confidence = Some(-2.0);
        let mut c = Segment::new(7, 2.0, 3.0, "低置信丙");
        c.confidence = Some(-2.0);
        let r = quality_report(&[a, b, c], &[], -0.35, false);
        assert_eq!(r.low_confidence, vec![3, 7, 7], "bucket 原样保留重复");
        assert_eq!(
            QualityCategory::LowConfidence.indices(&r),
            vec![3, 7],
            "取句列表时去重，顺序不动"
        );
    }

    /// 空报告：四类都取不到任何句，`indices` 全空。
    #[test]
    fn quality_category_indices_empty_for_empty_report() {
        let r = quality_report(&[], &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        for cat in QualityCategory::ALL {
            assert!(cat.indices(&r).is_empty(), "{:?} 不该有句", cat);
        }
    }

    /// 没有当前句时，「下一处」落在该类第一处。
    #[test]
    fn next_issue_in_none_returns_first() {
        assert_eq!(next_issue_in(None, &[3, 7, 9]), Some(3));
    }

    /// 当前句本身就是问题句：要**跳过**它而不是再返回一次（否则按一下不动）。
    #[test]
    fn next_issue_in_skips_current_issue() {
        assert_eq!(next_issue_in(Some(3), &[3, 7, 9]), Some(7));
    }

    /// 当前句落在两个问题句之间：取后面最近的一个。
    #[test]
    fn next_issue_in_between_issues_returns_next() {
        assert_eq!(next_issue_in(Some(8), &[3, 7, 9]), Some(9));
    }

    /// 已经在最后一处：回卷到第一处，循环翻查。
    #[test]
    fn next_issue_in_wraps_at_end() {
        assert_eq!(next_issue_in(Some(9), &[3, 7, 9]), Some(3));
    }

    /// 空分类：无论当前句是什么都没有「下一处」。
    #[test]
    fn next_issue_in_empty_slice_is_none() {
        assert_eq!(next_issue_in(Some(5), &[]), None);
        assert_eq!(next_issue_in(None, &[]), None);
    }

    /// 展示顺序与胶囊文案：顺序固定为 低置信 → 术语 → 未翻译 → 碎片句。
    #[test]
    fn quality_category_all_order_and_labels() {
        assert_eq!(
            QualityCategory::ALL,
            [
                QualityCategory::LowConfidence,
                QualityCategory::GlossaryViolation,
                QualityCategory::Untranslated,
                QualityCategory::EmptyOrShort,
            ]
        );
        assert_eq!(QualityCategory::LowConfidence.label(), "低置信");
        assert_eq!(QualityCategory::GlossaryViolation.label(), "术语违规");
        assert_eq!(QualityCategory::Untranslated.label(), "未翻译");
        assert_eq!(QualityCategory::EmptyOrShort.label(), "空/超短句");
    }

    // ==================== 词级时间戳 ====================

    /// WordTiming / Segment.words 的 serde 往返：词序与时间精度都不得丢。
    #[test]
    fn word_timings_serde_round_trip() {
        let mut seg = Segment::new(1, 0.0, 1.25, "你好");
        assert!(!seg.has_word_timings(), "new() 出来的段默认无词级信息");
        seg.words = vec![
            WordTiming {
                word: "你".to_string(),
                start: 0.0,
                end: 0.5,
            },
            WordTiming {
                word: "好".to_string(),
                start: 0.5,
                end: 1.25,
            },
        ];
        assert!(seg.has_word_timings());

        let json = serde_json::to_string(&seg).expect("序列化 Segment 失败");
        assert!(
            json.contains("words"),
            "words 必须随 Segment 一起序列化: {json}"
        );
        let back: Segment = serde_json::from_str(&json).expect("反序列化 Segment 失败");
        assert_eq!(back, seg);
        assert_eq!(back.words, seg.words, "词级时间戳往返后必须逐条一致");
    }

    /// 旧 JSON（无 words 字段）必须仍能反序列化，且 words 为空——向后兼容是硬要求。
    #[test]
    fn segment_without_words_field_deserializes_to_empty() {
        let legacy = r#"{
            "index": 3,
            "start": 1.0,
            "end": 2.0,
            "text": "旧记录",
            "translation": null,
            "translation_lang": null,
            "polished": "",
            "language": "zh",
            "confidence": -0.5,
            "speaker": 1
        }"#;
        let seg: Segment = serde_json::from_str(legacy).expect("旧 JSON 必须仍可反序列化");
        assert!(seg.words.is_empty(), "缺失 words 字段时按空表处理");
        assert!(!seg.has_word_timings());
        // 空表也要能再序列化回去，且字段存在（下游可稳定读到 words）
        let back = serde_json::to_value(&seg).unwrap();
        assert_eq!(back["words"], serde_json::json!([]));
    }
}
