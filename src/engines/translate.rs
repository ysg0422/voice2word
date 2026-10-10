//! 字幕翻译引擎 — 支持离线 Qwen 大模型字幕翻译及在线 API 极速翻译
//!
//! 两条链路共用同一套「逐行 `[序号] 译文`」协议与解析器：
//! - `OfflineQwen`：本地 llama.cpp + Qwen 小模型，免费、断网可用、无需密钥；
//! - `OnlineApi`：任意 OpenAI 兼容的 `/chat/completions` 接口
//!   （DeepSeek / OpenAI / 通义 / Kimi / 本地 vLLM / Ollama 均可）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::{debug, info, warn};

use crate::engines::llm::{
    chinese_variant_target, convert_chinese_variant, is_untranslated_copy, CopyRate,
};
use crate::engines::LLMEngine;
use crate::subtitle::Segment;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TranslateMode {
    /// 本地 Qwen 大模型离线翻译 (免费、无网可用)
    #[default]
    OfflineQwen,
    /// 在线 API 兼容格式 (OpenAI / DeepSeek 等)
    OnlineApi,
}

impl TranslateMode {
    /// 写入 config.toml 的稳定标识
    pub fn as_str(self) -> &'static str {
        match self {
            TranslateMode::OfflineQwen => "offline_qwen",
            TranslateMode::OnlineApi => "online_api",
        }
    }

    pub fn parse_mode(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "online" | "online_api" | "api" => TranslateMode::OnlineApi,
            _ => TranslateMode::OfflineQwen,
        }
    }

    /// 界面上展示的档位名
    pub fn label(self) -> &'static str {
        match self {
            TranslateMode::OfflineQwen => "本地 Qwen",
            TranslateMode::OnlineApi => "在线 API",
        }
    }
}

/// 在线 OpenAI 兼容接口的连接参数（与 `config.toml` 的 `[translate]` 一一对应）
#[derive(Debug, Clone)]
pub struct OnlineApiConfig {
    /// 已拼好的 `/chat/completions` 完整地址
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub batch_size: usize,
    pub timeout_secs: u64,
    /// 术语表提示（由 `TranslateConfig::glossary_prompt` 生成，空串表示无）。
    /// 离线与在线两条链路共用同一份注入文本。
    pub glossary_hint: String,
}

/// 在线模型能力画像：是否为「推理模型」（先输出思考、再输出译文）。
///
/// 一次整片翻译共享一份（`Arc`），因为**首批**才能暴露对面是不是推理模型，而
/// 后续所有批次（含折半补译）都要按这个结论调整预算与批大小。每个
/// [`TranslateEngine`] 任务各持一份，同进程内并发翻译不会互相污染。
#[derive(Debug, Default)]
struct ModelProfile {
    /// 是否已判定为推理模型（响应里出现过非空 `reasoning_content`）。
    reasoning: AtomicBool,
    /// 首批是否已跑完（跑完才谈得上「据此调整剩余批次」）。
    probed: AtomicBool,
    /// 观察到的思考文本长度（字符），诊断用。
    reasoning_chars: AtomicUsize,
    /// 「推理较慢」警告是否已打过（每次任务最多一条，避免刷屏）。
    slow_warned: AtomicBool,
}

impl ModelProfile {
    fn is_reasoning(&self) -> bool {
        self.reasoning.load(Ordering::Relaxed)
    }

    /// 标记为推理模型并记录思考长度；返回是否**首次**判定（用于只打一条日志）。
    fn mark_reasoning(&self, chars: usize) -> bool {
        self.reasoning_chars.fetch_max(chars, Ordering::Relaxed);
        !self.reasoning.swap(true, Ordering::Relaxed)
    }

    fn reasoning_chars(&self) -> usize {
        self.reasoning_chars.load(Ordering::Relaxed)
    }

    fn is_probed(&self) -> bool {
        self.probed.load(Ordering::Relaxed)
    }

    fn mark_probed(&self) {
        self.probed.store(true, Ordering::Relaxed);
    }

    fn mark_slow_warned(&self) -> bool {
        !self.slow_warned.swap(true, Ordering::Relaxed)
    }
}

pub struct TranslateEngine {
    mode: TranslateMode,
    llm_engine: Option<LLMEngine>,
    online: Option<OnlineApiConfig>,
}

impl TranslateEngine {
    /// 离线 Qwen 翻译引擎（等价于 [`TranslateEngine::offline`]）
    pub fn new(llm_engine: LLMEngine) -> Self {
        Self::offline(llm_engine)
    }

    pub fn offline(llm_engine: LLMEngine) -> Self {
        Self {
            mode: TranslateMode::OfflineQwen,
            llm_engine: Some(llm_engine),
            online: None,
        }
    }

    pub fn online(config: OnlineApiConfig) -> Self {
        Self {
            mode: TranslateMode::OnlineApi,
            llm_engine: None,
            online: Some(config),
        }
    }

    pub fn mode(&self) -> TranslateMode {
        self.mode
    }

    /// 批量翻译字幕片段为目标语言（默认 "简体中文"）
    pub fn translate_subtitles(
        &self,
        segments: Vec<Segment>,
        target_lang: &str,
        progress_cb: Option<crate::engines::TextProgressCb>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<Segment>> {
        info!(
            "开始执行字幕翻译 (目标语言: {}, 引擎: {:?})",
            target_lang, self.mode
        );
        // P0-A：目标语言是**中文变体**时短路，不走 LLM。
        //
        // 本项目源字幕本来就是中文，而目标清单里含「简体中文 / 繁体中文」。若交给
        // 小模型，提示词会变成自相矛盾的「源语言为中文；……翻译为地道的繁体中文。」，
        // 它最省力的输出就是**原样复制**——实测 8/8 句与原文一模一样，界面却显示
        // 「翻译已完成（8 句）」。中文变体之间是确定性字符映射，用 zhconv 直接转换
        // 即可（简→繁 / 繁→简），既准确又不花推理时间。
        //
        // 例外：目标「简体中文」而源本来就是简体（绝大多数情况）时，转换结果 == 原文。
        // 这是**正确行为**（中文→简体中文本就该是原文），不是失败，因此这里照常写入
        // `translation`，不参与「译文==原文」的复制检测。
        if let Some(variant) = chinese_variant_target(target_lang) {
            return convert_chinese_variant(segments, target_lang, variant, progress_cb, &cancel);
        }
        match self.mode {
            TranslateMode::OfflineQwen => {
                let engine = self
                    .llm_engine
                    .as_ref()
                    .ok_or_else(|| anyhow!("离线翻译引擎未初始化"))?;
                // P0-A（离线侧）：中文变体目标同样短路——`LLMEngine::translate` 是
                // 公开入口，可能被直接调用，判定下沉到那里同样成立（见
                // `LLMEngine::translate` 的短路分支）。
                if let Some(variant) = chinese_variant_target(target_lang) {
                    return convert_chinese_variant(
                        segments,
                        target_lang,
                        variant,
                        progress_cb,
                        &cancel,
                    );
                }
                // 取消标志下沉到引擎：润色/翻译都是分批长循环，取消必须能在批次之间生效
                engine.install_cancel_flag(cancel);
                engine.translate(segments, target_lang, progress_cb)
            }
            TranslateMode::OnlineApi => {
                let cfg = self
                    .online
                    .as_ref()
                    .ok_or_else(|| anyhow!("在线翻译接口未配置"))?;
                translate_via_online_api(cfg, segments, target_lang, progress_cb, cancel)
            }
        }
    }

    /// 连通性自检：向在线接口发一条极短请求，成功返回模型回显文本。
    /// 供设置页「测试连接」按钮使用，避免用户等到整片翻译失败才发现密钥写错。
    pub fn probe_online(&self) -> Result<String> {
        let cfg = self
            .online
            .as_ref()
            .ok_or_else(|| anyhow!("当前不是在线 API 模式"))?;
        let payload = serde_json::json!({
            "model": cfg.model,
            "messages": [
                { "role": "user", "content": "只回复两个字：正常" }
            ],
            "temperature": 0.0,
            // 探测也要给足预算：推理模型（如 deepseek-v4）会先花几百 token 思考，
            // 之前只给 16，思考还没结束就被截断，反而把思考过程当成回显给用户。
            "max_tokens": 512,
            "stream": false,
        });
        let value = post_chat_completions(cfg, &payload)?;
        Ok(extract_message_content(&value)
            .unwrap_or_else(|| "(接口返回成功，但未解析到文本内容)".to_string()))
    }
}

/// 在线翻译主流程：按 `batch_size` 切片，逐批请求并回填 `segment.translation`。
fn translate_via_online_api(
    cfg: &OnlineApiConfig,
    mut segments: Vec<Segment>,
    target_lang: &str,
    progress_cb: Option<crate::engines::TextProgressCb>,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<Segment>> {
    if cfg.api_key.trim().is_empty() {
        return Err(anyhow!(
            "未配置在线翻译 API Key。请在「性能设置 → 在线翻译 API」中填写，\
             或设置环境变量 VOICE2WORD_API_KEY"
        ));
    }
    if cfg.model.trim().is_empty() {
        return Err(anyhow!(
            "未配置在线翻译模型名（如 deepseek-chat / gpt-4o-mini）"
        ));
    }
    // 接口地址为空时，`chat_completions_url()` 会拼出 "/chat/completions"——
    // ureq 会报一句难懂的 URL 解析错误。这里提前给出可操作的中文提示。
    if !cfg.endpoint.starts_with("http://") && !cfg.endpoint.starts_with("https://") {
        return Err(anyhow!(
            "在线翻译接口地址不合法：{}。请在「性能设置 → 在线翻译 API」中填写完整的基址（如 https://api.deepseek.com/v1）。",
            cfg.endpoint
        ));
    }

    let total = segments.len();
    let batch_size = cfg.batch_size.clamp(1, 60);
    info!(
        "在线 API 翻译 {} 条字幕为 {}（模型 {}，每批 {} 条）",
        total, target_lang, cfg.model, batch_size
    );
    if let Some(ref cb) = progress_cb {
        cb(0.0, &format!("正在连接在线翻译接口 ({})...", cfg.model));
    }

    // 只译「还没有目标语言译文」的句子：与离线链路同一套增量语义。
    // 在线接口按 token 计费，重复翻译已完成的句子是直接烧钱。
    let translatable: Vec<usize> = segments
        .iter()
        .enumerate()
        .filter(|(_, seg)| {
            !seg.translate_source().trim().is_empty() && !seg.translation_matches(target_lang)
        })
        .map(|(pos, _)| pos)
        .collect();

    let pending_total = translatable.len();
    if pending_total == 0 {
        info!("所有片段均已是 {target_lang} 译文，无需请求在线接口");
        if let Some(ref cb) = progress_cb {
            cb(1.0, &format!("全部 {total} 句已是{target_lang}译文"));
        }
        return Ok(segments);
    }
    if pending_total < total {
        info!("在线增量翻译：{total} 句中有 {pending_total} 句需要翻译为 {target_lang}");
    }

    let mut completed = 0usize;
    // 失败但**不需要中止整片**的批次数（重试用尽、或本批解析不完整）
    let mut soft_failed_batches = 0usize;
    // P0-B：整批的「译文 == 原文」复制率统计
    let mut copy = CopyRate::default();

    // P0-C：单批源文本预算必须扣掉**每批重复注入**的固定开销——system 提示词模板
    // （含源语言线索与术语表）。术语表理论上限约 9.7k 字符，不扣就会顶爆小上下文
    // 服务端（400）或让输出被截断。
    let fixed_chars =
        ONLINE_SYSTEM_PROMPT_CHARS + cfg.glossary_hint.chars().count() + ONLINE_SRC_HINT_CHARS;
    let char_budget = online_char_budget(fixed_chars);

    // P0-2：推理模型的思考 token 与译文共享 `max_tokens`，必须缩批 + 加大预算。
    // `profile` 在整片翻译内共享：首批响应里出现非空 `reasoning_content` 即判定
    // 对面是推理模型，随后把**剩余**句子按更小的批重排。
    let profile = Arc::new(ModelProfile::default());
    let mut batch_size = batch_size;
    let mut queue: VecDeque<Vec<usize>> =
        plan_online_batches(&translatable, &segments, batch_size, char_budget).into();

    while let Some(chunk) = queue.pop_front() {
        // 每批开始前检查取消：单批最长可达 `timeout_secs`（默认 120s），
        // 不在批次之间检查的话，用户点「取消」要等当前批跑完才生效。
        if cancel.load(Ordering::Relaxed) {
            info!("在线翻译被取消，保留已完成的部分（{completed}/{pending_total}）");
            break;
        }
        let batch: Vec<&Segment> = chunk.iter().map(|&pos| &segments[pos]).collect();
        // 单批失败重试：整片刻一两千句、每批几十次请求，任何一次 429/网络抖动
        // 都会让「整片翻译」前功尽弃。重试有上限，超限后跳过本批继续后面的句子，
        // 把已经译好的部分保住（与取消时的语义一致：返回偏序结果而非丢弃）。
        let translations = match request_batch_with_retry(
            cfg,
            &batch,
            target_lang,
            &cancel,
            &profile,
        ) {
            Ok(map) => map,
            Err(err) => {
                // 取消（而非真实错误）：保留已完成的部分，按取消语义返回偏序结果。
                // 这一步必须排在 is_retryable 之前——用户取消会以「翻译已取消」
                // 从这里返回，若不特判就会被当成「不可重试错误」直接报失败。
                if cancel.load(Ordering::Relaxed) {
                    info!("在线翻译被取消，保留已完成的部分（{completed}/{pending_total}）");
                    break;
                }
                // 参数类错误（401/403/404：密钥、地址、模型名）不是暂时性的，
                // 后面的批会以完全相同的原因再失败一遍。继续「逐批跳过」只会把
                // 一个配置错误伪装成「翻译完成 0 句」。立刻带着真实原因中止。
                if !is_retryable(&err) {
                    return Err(err).map_err(|e| {
                        anyhow!("在线翻译中止：{e}。请检查「性能设置 → 在线翻译 API」的地址、密钥与模型名。")
                    });
                }
                soft_failed_batches += 1;
                warn!(
                    error = %err,
                    "在线翻译本批失败（已重试 {} 次），跳过该批继续后续句子",
                    MAX_BATCH_RETRIES
                );
                completed += chunk.len();
                if let Some(ref cb) = progress_cb {
                    let progress = completed as f64 / pending_total.max(1) as f64;
                    cb(
                        progress,
                        &format!(
                            "在线翻译中: {completed}/{pending_total} 条（{} 批失败，已跳过）",
                            soft_failed_batches
                        ),
                    );
                }
                continue;
            }
        };

        // P0-2：首批响应即可判定对面是不是推理模型；命中后把**剩余**句子按更小的
        // 批重排——推理模型的思考量会随批大小与预算上浮，大批很容易再次撞满预算。
        if !profile.is_probed() {
            profile.mark_probed();
            if profile.is_reasoning() {
                batch_size = batch_size.min(REASONING_BATCH_LINES);
                let remaining: Vec<usize> = queue.iter().flatten().copied().collect();
                queue = plan_online_batches(&remaining, &segments, batch_size, char_budget).into();
                info!(
                    "检测到推理模型（思考过程约 {} 字符），每批条数 {batch_size}，\
                     输出预算提高到「译文 + {REASONING_THINKING_TOKENS}」以容纳思考过程",
                    profile.reasoning_chars()
                );
            }
        }
        let mut matched = 0usize;
        // 被判为「复制原文」的句子：折半重试**之后**仍未译出才计入复制率。
        let mut copy_candidates: Vec<usize> = Vec::new();
        copy.checked += chunk.len();
        for &pos in &chunk {
            if let Some(text) = translations.get(&segments[pos].index) {
                // P0-B：模型把原文原样吐回来时不算译出——走已有的折半重试路径补译。
                if is_untranslated_copy(&segments[pos].translate_source(), text) {
                    copy_candidates.push(pos);
                    continue;
                }
                segments[pos].translation = Some(text.clone());
                segments[pos].translation_lang = Some(target_lang.to_string());
                matched += 1;
            }
        }
        if matched < chunk.len() {
            // 本批没译全：把漏掉的句子**对半重试**，而不是整批丢弃。
            // 输出被截断时，拆成更小的批通常一轮就能补齐；实在补不上才计入软失败。
            let missing: Vec<usize> = chunk
                .iter()
                .copied()
                .filter(|&pos| !segments[pos].translation_matches(target_lang))
                .collect();
            let recovered =
                retry_missing_online(cfg, &mut segments, &missing, target_lang, &cancel, &profile);
            if recovered < missing.len() {
                soft_failed_batches += 1;
                warn!(
                    "在线翻译本批输出不完整（{matched}/{} 条，折半重试后补回 {recovered} 条）",
                    chunk.len()
                );
            }
        }
        // 重试之后仍没译出的复制句，才真正计入复制率。
        copy.rejected += copy_candidates
            .iter()
            .filter(|&&pos| !segments[pos].translation_matches(target_lang))
            .count();
        completed += chunk.len();
        if let Some(ref cb) = progress_cb {
            let progress = completed as f64 / pending_total.max(1) as f64;
            cb(
                progress,
                &format!(
                    "在线翻译中: {completed}/{pending_total} 条（{}）",
                    cfg.model
                ),
            );
        }
    }

    // 一句都没译出、又没被取消：多半是接口配置/鉴权问题被分批重试掩盖成了
    // 「逐批跳过」。此时返回 `Ok` 会让上层显示「翻译已完成（0 句）」——假成功。
    // 这里显式报错，让用户看到真正的失败原因（密钥、地址、模型名）。
    if !cancel.load(Ordering::Relaxed) {
        let ok = translatable
            .iter()
            .filter(|&&pos| segments[pos].translation_matches(target_lang))
            .count();
        // P0-B 兜底：复制率过半说明模型整体在复制原文，不能静默返回 Ok 让界面显示
        // 「翻译已完成（N 句）」的假成功。**排在「一句都没译出」之前**：整篇复制时
        // 它才是真正的原因，比笼统的「未能译出任何一句」更能指导用户换模型。
        if copy.is_alarming() {
            warn!(
                rejected = copy.rejected,
                checked = copy.checked,
                "在线翻译复制率过高（{:.0}%）：译文与原文相同，疑似模型未真正翻译",
                copy.rate() * 100.0
            );
            return Err(anyhow!(
                "在线翻译质量异常：{}/{} 句译文与原文完全相同（复制率 {:.0}%），已按「未译出」处理。请检查模型是否适合该目标语言（可换更大的模型）。",
                copy.rejected,
                copy.checked,
                copy.rate() * 100.0
            ));
        }
        if ok == 0 {
            return Err(anyhow!(
                "在线翻译未能译出任何一句（共 {pending_total} 句待译）。请用「性能设置 → 在线翻译 API → 测试连接」检查地址、密钥与模型名。"
            ));
        }
        if copy.rejected > 0 {
            warn!(
                rejected = copy.rejected,
                checked = copy.checked,
                "在线翻译有 {} 句译文与原文相同，已判为未译出",
                copy.rejected
            );
        }
    }
    if soft_failed_batches > 0 {
        info!(
            "在线 API 字幕翻译完成（{soft_failed_batches} 批未成功，可再次点击「开始翻译」补译）"
        );
    } else {
        info!("在线 API 字幕翻译完成");
    }
    Ok(segments)
}

/// 把一批里**没译出**的句子按更小的批重试，返回补回的条数。
///
/// 与离线链路同一套思路：输出被截断时整批丢弃会让用户看到「翻译少了几句」，
/// 拆成更小的批（折半）通常一轮就能补齐。最多折半到单句；对某句反复补不上
/// （模型就是吐不出该序号）则放弃，交由上层计入软失败并提示用户可再点一次补译。
fn retry_missing_online(
    cfg: &OnlineApiConfig,
    segments: &mut [Segment],
    missing: &[usize],
    target_lang: &str,
    cancel: &AtomicBool,
    profile: &ModelProfile,
) -> usize {
    if missing.is_empty() {
        return 0;
    }
    let mut recovered = 0usize;
    let mut todo: Vec<usize> = missing.to_vec();
    let mut guard = 0;
    while !todo.is_empty() && guard < 8 {
        guard += 1;
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let half = todo.len().div_ceil(2);
        let mut still: Vec<usize> = Vec::new();
        for sub in todo.chunks(half) {
            if cancel.load(Ordering::Relaxed) {
                return recovered;
            }
            let batch: Vec<&Segment> = sub.iter().map(|&pos| &segments[pos]).collect();
            match request_batch_with_retry(cfg, &batch, target_lang, cancel, profile) {
                Ok(map) => {
                    for &pos in sub {
                        if let Some(text) = map.get(&segments[pos].index) {
                            // 折半后仍原样复制 → 就是没译出：保持未译，计入软失败。
                            if is_untranslated_copy(&segments[pos].translate_source(), text) {
                                still.push(pos);
                            } else {
                                segments[pos].translation = Some(text.clone());
                                segments[pos].translation_lang = Some(target_lang.to_string());
                                recovered += 1;
                            }
                        } else {
                            still.push(pos);
                        }
                    }
                }
                // 请求整体失败：不再继续折半（多半是网络/限流，继续只会更糟），
                // 把剩余的留给上层计入软失败。
                Err(_) => {
                    still.extend_from_slice(sub);
                }
            }
        }
        if still.len() >= todo.len() {
            break;
        }
        todo = still;
    }
    recovered
}

/// 单批重试上限。选 3 而不是更多：429 限流通常需要数秒才恢复，再多轮也是空转，
/// 不如跳过本批让后面的句子先译完，用户可再点一次「开始翻译」补齐。
const MAX_BATCH_RETRIES: usize = 3;

/// 带重试地请求一批译文。仅对**可重试**错误重试（网络/限流/5xx），
/// 参数类错误（401/403/404）立刻返回，重试只是浪费用户时间。
fn request_batch_with_retry(
    cfg: &OnlineApiConfig,
    batch: &[&Segment],
    target_lang: &str,
    cancel: &AtomicBool,
    profile: &ModelProfile,
) -> Result<HashMap<usize, String>> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..MAX_BATCH_RETRIES {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("翻译已取消"));
        }
        match request_batch_translation(cfg, batch, target_lang, profile) {
            Ok(map) => return Ok(map),
            Err(err) => {
                if !is_retryable(&err) {
                    return Err(err);
                }
                warn!(attempt = attempt + 1, total = MAX_BATCH_RETRIES, error = %err, "在线翻译请求失败，准备重试");
                last_err = Some(err);
                // 线性退避：失败往往是限流，立刻重发只会继续被拒。
                // 取消标志已在上面的循环处检查，这里的等待是可中断的短睡。
                std::thread::sleep(Duration::from_millis(600 * (attempt as u64 + 1)));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("在线翻译请求失败")))
}

/// 错误是否值得重试：网络层错误与 429 / 5xx 属于暂时性，其余（401/403/404 等）不是。
fn is_retryable(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    // 优先按真实状态码判断。不能直接 `contains("HTTP 5")`：因为错误文案里还拼了服务端的
    // 响应体（snippet），一个 4xx 的响应体若恰好提到了「HTTP 5xx」，就会被误判为可重试而白等退避。
    if let Some(code) = http_status_code(&msg) {
        return code == 429 || (500..=599).contains(&code);
    }
    // 无状态码：ureq 的网络类错误，文案里带「无法连接」或底层 io 错误
    msg.contains("无法连接") || msg.contains("timed out") || msg.contains("timeout")
}

/// 从错误文案里取出「HTTP <状态码>」的状态码。取不到返回 `None`。
///
/// 只认**紧跟在 "HTTP " 之后的 3 位数字**：避开响应体里其他位置的数字（如日志里的流水号）。
fn http_status_code(msg: &str) -> Option<u16> {
    let idx = msg.find("HTTP ")?;
    let rest = &msg[idx + "HTTP ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 3 {
        digits.parse::<u16>().ok()
    } else {
        None
    }
}

/// 把待译下标切成在线请求的批次：**同时**受「条数」与「字符数」约束。
///
/// 离线链路早已按字符预算切批（见 `llm::plan_translate_batches`），在线链路此前
/// 只按条数切。后果：一小批 40 条**长字幕**拼出的 prompt 可能顶爆小上下文服务端
/// （本地 vLLM / Ollama 或 32K 以外的自建网关），服务端返回 400；而 `is_retryable`
/// 把 4xx 判为**不可重试**，于是整片翻译在第一批就带着「在线翻译中止」失败。
///
/// 这里复用同一套字符预算思路：单批源文本（含 `[序号] ` 前缀）超过 `char_budget`
/// （由 [`online_char_budget`] 按固定开销折算）就提前收口，单条自身超预算时仍单独成批
/// （交回服务端处理总好过在这里死循环）。
fn plan_online_batches(
    pending: &[usize],
    segments: &[Segment],
    max_lines: usize,
    char_budget: usize,
) -> Vec<Vec<usize>> {
    let max_lines = max_lines.max(1);
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_chars = 0usize;
    for &pos in pending {
        // 序号文本（"[123] "）也算输入，粗估 6 字符
        let cost = segments[pos].translate_source().chars().count() + 6;
        let would_overflow = !current.is_empty()
            && (current.len() >= max_lines || current_chars + cost > char_budget);
        if would_overflow {
            batches.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current.push(pos);
        current_chars += cost;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// 在线链路**不含固定开销**时单批源文本的字符上限（含 `[序号] ` 前缀）。
///
/// 推导：按最保守的 8K token 上下文估算——中文约 1 token/字符，一批要同时装下
/// 「输入 prompt」与「模型输出」，于是单批源文本与译文各约 8192 / 2 = 4096 字符；
/// 再压到 3500 留安全余量。
///
/// 注意这只是**安全上限**：普通字幕（每行 < 60 字）即便 40 行也才 ~2400 字符，
/// 根本够不到预算，因此常规批次仍由条数上限（20/40 条）决定，不会变慢；
/// 只有出现**超长行**时预算才会提前收口——那正是顶爆小上下文服务端的场景。
const ONLINE_CHAR_CAP: usize = 3_500;

/// 在线 system 提示词模板（含格式约束）的字符数：约 130 汉字。
const ONLINE_SYSTEM_PROMPT_CHARS: usize = 130;
/// 在线源语言线索（`源语言为中文；`）的字符数。
const ONLINE_SRC_HINT_CHARS: usize = 10;

/// 在线链路单批的**可用源文本字符预算**：从 [`ONLINE_CHAR_CAP`] 里扣掉
/// **每批重复注入**的 system 提示词（含源语言线索与术语表）的实际长度。
///
/// 术语表（`config.rs` 允许 80 条 × 左右各 60 字符 ≈ 9.7k 字符）若不计入，
/// 一批字幕加提示词就会顶爆小上下文服务端（返回 400，而 4xx 被 `is_retryable`
/// 判为不可重试 → 整片翻译第一批就中止）。这里让预算显式感知它。
fn online_char_budget(fixed_chars: usize) -> usize {
    ONLINE_CHAR_CAP.saturating_sub(fixed_chars).max(1_024)
}

/// 一条字幕译文大致需要的输出 token（本地 Qwen2.5 实测中→英约 0.84 token/字符，
/// 按 1.0 封顶给膨胀型目标语言留余量）。
const OUTPUT_TOKENS_PER_CHAR: usize = 1;
/// 输出 token 的硬下限。
///
/// 不能只算“译文本身需要多少 token”——很多在线模型是**推理模型**，
/// 会先花几百 token “思考”再输出译文，而这些思考 token 也占 `max_tokens`。
/// 下限太低时，模型思考到一半就被截断，译文根本没开始写。
/// 实测（某推理型模型，三条短字幕）：思考占 ~184 token、译文 ~50 token。
/// 取 512 作为下限，给推理模型留出足够的思考空间。
const MIN_OUTPUT_TOKENS: usize = 512;

/// 非推理模型的输出 token 宽松上限：防个别服务对超大值报错。
///
/// 复核依据：这是按「8K 上下文」推出来的保守值，**只对普通 chat 模型**成立。
/// 推理模型的上限大得多（网关 `/v1/models` 实测报 `max_output_tokens = 393216`），
/// 8192 根本不够它把思考 + 译文写完，因此推理模型走
/// [`MAX_REASONING_OUTPUT_TOKENS`]，不受这里限制。
const MAX_OUTPUT_TOKENS: usize = 8_192;

/// 推理模型单批的「思考余量」token。
///
/// 实测（某网关的推理型模型，`only_reasoning: true`，且思考不省 token）：
/// - 12 句标准字幕一批：`max_tokens=512` → 思考 512 就把预算吃光，
///   `finish_reason=length`、`content` 为空（0/12 译出）；2048 → 思考 2048，同样 0/12；
///   4096 → 思考 4089，`content` 只剩 `[1] 皆さん` 一行；8192 → 思考 2.6k~6.4k，12/12。
///   可见思考量会**随预算上浮**，预算必须给足，不能靠 clamp 从译文预算里挤。
/// - 单批思考实测区间 1.4k~6.0k token。
///
/// 取 8192：覆盖实测思考峰值（~6k）后仍给译文留 ~2k，且远低于该网关 393216
/// 的输出上限，服务端接受。
const REASONING_THINKING_TOKENS: usize = 8_192;

/// 推理模型单批输出预算上限。
///
/// 实测 16384 / 32768 均被服务端接受，但 32768 时思考涨到 5.5k、单批耗时翻倍——
/// 预算给得越大，模型越想得越久。取 16384：够「思考余量 + 长批译文」，又不会
/// 让每批明显变慢。
const MAX_REASONING_OUTPUT_TOKENS: usize = 16_384;

/// 推理模型每批最大条数：把思考摊薄到更小的批。
///
/// 实测 12 句/8192 可以 12/12，但思考量波动大（1.4k~6.0k），批越大越容易撞上
/// 单批预算。压到 6 条：单批源文本 ~150 字符，预算 = 译文 + 8192 思考余量，
/// 实测 5/6/8 条 × 8192 全部 `finish_reason=stop`、条条译全。
const REASONING_BATCH_LINES: usize = 6;

/// 按本批源文本长度推导 `max_tokens`。
///
/// 在线接口不显式给 `max_tokens` 时，部分服务端默认值偏小（如 512），长批会**静默
/// 截断**——返回的译文行数少于请求行数，而 `parse_batch_response` 只认能解析出的行，
/// 少掉的那几句就原样留空。这里按源字符数给足预算（不再依赖服务端默认），
/// 同时用一个宽松上限防止个别服务对超大值报错。
///
/// P0-C：`prompt_chars` 是**固定开销**（system 提示词模板 + 源语言线索 + 术语表）的
/// 字符数——它和源文本一起占用上下文，必须从「源文本 + 输出」之外扣掉，否则术语表
/// 一长，源文本的 token 预算就会被高估，输出被服务端截断。
///
/// P0-2：`reasoning` 为真表示已判定对面是推理模型——思考 token 与译文共享
/// `max_tokens`，此时改用「译文预算 + [`REASONING_THINKING_TOKENS`]」，
/// 不再受 8K 上下文裁剪（推理模型上下文远大于 8K）。
fn max_output_tokens(batch: &[&Segment], prompt_chars: usize, reasoning: bool) -> usize {
    let chars: usize = batch
        .iter()
        .map(|seg| seg.translate_source().chars().count())
        .sum();
    let wanted = chars * OUTPUT_TOKENS_PER_CHAR;
    if reasoning {
        // 思考量随预算上浮（512→512、4096→4089），所以是「译文预算 **加上**
        // 一笔思考余量」，而不是把译文预算 clamp 大——后者仍会被思考吃光。
        return wanted
            .saturating_add(REASONING_THINKING_TOKENS)
            .clamp(MIN_OUTPUT_TOKENS, MAX_REASONING_OUTPUT_TOKENS);
    }
    // 最保守地按 8K 上下文与「中文约 1 token/字符」估算可用空间：
    // 上下文 - （固定开销 + 源文本）就是能给输出的部分，再夹在 [MIN, MAX] 之间。
    let available = 8_192usize.saturating_sub(prompt_chars + chars);
    wanted.clamp(
        MIN_OUTPUT_TOKENS,
        MAX_OUTPUT_TOKENS.min(available.max(MIN_OUTPUT_TOKENS)),
    )
}

/// 请求一批字幕的译文，返回 `序号 -> 译文` 映射。
///
/// `profile` 承载「对面是不是推理模型」的判定：本次响应一旦出现非空
/// `reasoning_content` 就置位，并让 `max_tokens` 立刻按推理模型给足。
fn request_batch_translation(
    cfg: &OnlineApiConfig,
    batch: &[&Segment],
    target_lang: &str,
    profile: &ModelProfile,
) -> Result<HashMap<usize, String>> {
    let source = batch
        .iter()
        .map(|seg| format!("[{}] {}", seg.index, seg.translate_source()))
        .collect::<Vec<_>>()
        .join("\n");

    // 源语言线索：ASR 已检测到整批语言（Segment::language），多给这一条能减少
    // 「日→中」「中英混排」方向上的漏译与专名误翻。
    let src_hint = crate::subtitle::dominant_language(batch.iter().copied())
        .map(|code| format!("源语言为{}；", crate::subtitle::language_name(&code)))
        .unwrap_or_default();
    let payload = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {
                "role": "system",
                "content": format!(
                    "你是专业影视字幕翻译专家。{src_hint}{glossary}将用户给出的带序号字幕逐条翻译为地道的{target_lang}。\
【输出格式规范】\
必须且只能逐行输出，每行格式严格为「[序号] 译文」（严禁缺少括号与序号）。\
【硬性约束】\
1. 严禁添加任何问候、解释说明、Markdown代码块标记（如 ```）或前后赘述。\
2. 保持原意与语气，语言通顺紧凑，译文长度尽量与原文相当。\
3. 序号必须与原文完全一致，严禁遗漏任何一行，严禁合并多行，严禁改动序号。\
4. 数字、公式、变量名、编号与专有名词原样保留。",
                    glossary = cfg.glossary_hint
                )
            },
            { "role": "user", "content": source }
        ],
        "temperature": 0.2,
        "max_tokens": max_output_tokens(
            batch,
            ONLINE_SYSTEM_PROMPT_CHARS + cfg.glossary_hint.chars().count() + ONLINE_SRC_HINT_CHARS,
            profile.is_reasoning()
        ),
        "stream": false,
    });

    let mut value = post_chat_completions(cfg, &payload)?;

    // P0-2：判定对面是不是推理模型——响应里出现**非空** `reasoning_content`
    // 即命中（实测该网关即使 `enable_thinking:false` 也照常返回思考文本）。
    // 同时把思考长度记进日志，供 P1-3 诊断，**绝不**写进字幕。
    let observed = reasoning_content_len(&value);
    // 「本批之前」是否已经知道对面是推理模型。若已知，说明当前预算已是推理预算，
    // 只回思考就不再徒劳重试（重试会命中同样的预算）。
    let known_reasoning = profile.is_reasoning();
    if observed > 0 {
        let first = profile.mark_reasoning(observed);
        debug!(
            reasoning_chars = observed,
            first_detection = first,
            "在线翻译响应含推理模型思考过程（reasoning_content，仅用于诊断，不进入译文）"
        );
        // 思考量逼近预算时提醒一次：推理模型确实更慢、更贵。
        if observed >= REASONING_THINKING_TOKENS / 2 && profile.mark_slow_warned() {
            warn!(
                reasoning_chars = observed,
                "该模型是推理模型（思考过程较长，约 {observed} 字符），在线翻译会更慢、token 消耗更高"
            );
        }
    }

    let mut content = extract_message_content(&value).unwrap_or_default();
    // 没有 `content`、却明明有思考文本：几乎必然是 `max_tokens` 被思考吃光后截断。
    // 此时**不**把思考当译文，而是用「思考量 + 译文预算」的更大预算重试一次。
    if content.trim().is_empty() && observed > 0 && !known_reasoning {
        let retry_tokens = max_output_tokens(batch, 0, true);
        warn!(
            reasoning_chars = observed,
            retry_max_tokens = retry_tokens,
            "在线接口只返回了思考过程、没有译文，疑似输出预算不足，改用推理模型预算重试该批"
        );
        if let Some(mut p) = payload.as_object().cloned() {
            p.insert(
                "max_tokens".to_string(),
                serde_json::Value::from(retry_tokens),
            );
            value = post_chat_completions(cfg, &serde_json::Value::Object(p))?;
            let retry_observed = reasoning_content_len(&value);
            if retry_observed > 0 {
                profile.mark_reasoning(retry_observed);
            }
            content = extract_message_content(&value).unwrap_or_default();
        }
    }
    if content.trim().is_empty() {
        return Err(anyhow!(
            "在线接口返回了空内容。若使用的是推理模型（如 deepseek-reasoner），请改用非推理模型"
        ));
    }

    let expected: HashSet<usize> = batch.iter().map(|seg| seg.index).collect();
    let mut parsed = LLMEngine::parse_batch_response(&content, &expected);

    // 输出被截断（`finish_reason=length`）时，**最后一条几乎必然是被切一半的**（模型
    // 写到一半就撞上 max_tokens）。麻烦在于这种半截行**仍能被
    // `parse_batch_response` 当成一条完整译文**——于是 `matched == chunk.len()`，
    // 上层的“本批未译全”分支不会触发，用户看到的就是一句被截断的译文。
    //
    // 这里主动**丢掉最后一条已解析的译文**：它自带的“缺一条”会让上层走
    // 现有的折半重试（`retry_missing_online`）把它当成“漏译”补回来。丢掉的那条
    // 即使本来完整，重译一次也不会出错，只是多一次请求。
    if finish_reason_is_length(&value) {
        let last_matched = batch
            .iter()
            .rev()
            .map(|s| s.index)
            .find(|i| parsed.contains_key(i));
        if let Some(idx) = last_matched {
            parsed.remove(&idx);
            warn!(
                index = idx,
                batch = batch.len(),
                "在线翻译输出被截断，丢弃末条并交由折半重试补齐"
            );
        } else if profile.is_reasoning() {
            // 一条都没解析出来、对面又是推理模型：输出预算被思考吃光了，
            // `content` 里根本没有译文。折半重试会命中同样的预算，徒劳；
            // 这里显式报错，让上层按「本批失败」跳过而不是误判成「空内容」。
            return Err(anyhow!(
                "推理模型思考过程耗尽输出预算，本批未产出译文（thinking 约占 {REASONING_THINKING_TOKENS} token，可降低每批条数后重试）"
            ));
        }
    }

    Ok(parsed)
}

/// 响应的 `choices[0].finish_reason` 是否为 `length`（输出被 max_tokens 截断）。
///
/// 兼容两种位置：`chat/completions` 的 `choices[0].finish_reason`，
/// 以及部分网关放在 `choices[0].message` 下的同名字段。取不到时返回 `false`
/// （宁可当作没截断，也不要把正常响应误报成失败）。
fn finish_reason_is_length(value: &serde_json::Value) -> bool {
    let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
        return false;
    };
    let reason = choice
        .get("finish_reason")
        .and_then(|r| r.as_str())
        .or_else(|| {
            choice
                .get("message")
                .and_then(|m| m.get("finish_reason"))
                .and_then(|r| r.as_str())
        });
    reason
        .map(|r| r.eq_ignore_ascii_case("length"))
        .unwrap_or(false)
}

/// 统一 POST 到 `/chat/completions` 并解析 JSON 响应体
fn post_chat_completions(
    cfg: &OnlineApiConfig,
    payload: &serde_json::Value,
) -> Result<serde_json::Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(cfg.timeout_secs.clamp(10, 600)))
        .build();

    let response = agent
        .post(&cfg.endpoint)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", cfg.api_key.trim()))
        .send_json(payload.clone());

    match response {
        Ok(resp) => resp
            .into_json::<serde_json::Value>()
            .map_err(|e| anyhow!("在线接口响应不是合法 JSON: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            // 4xx/5xx 的响应体通常带服务端的错误说明，原样透出便于排查
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            let hint = match code {
                401 | 403 => "（API Key 无效或无权限）",
                404 => "（接口地址或模型名不存在）",
                429 => "（触发限流，请降低批量条数或稍后重试）",
                _ => "",
            };
            Err(anyhow!("在线接口返回 HTTP {code}{hint}: {snippet}"))
        }
        Err(e) => Err(anyhow!("无法连接在线翻译接口 {}: {e}", cfg.endpoint)),
    }
}

/// 从 OpenAI 兼容响应里取出回复文本。
/// 兼容两种形态：`chat/completions` 的 `message.content`、
/// 以及旧版 `completions` 的 `choices[0].text`。
///
/// **绝不回退到 `message.reasoning_content`。** 那是推理模型的**思考过程**，
/// 不是译文；把它当译文会直接污染字幕。实测（某网关的推理型模型）
/// （`only_reasoning: true`）的思考文本里混着大量元话语与候选方案，例如：
///
/// ```text
/// 这部分内容呢主要是两块 -> この部分の内容はですね、主に二つです。
/// 或 この内容は主に２つに分かれます。带"呢"语气：...
/// ```
///
/// 这种文本一旦回填进 `segment.translation`，用户看到的字幕里就带着
/// 「或…」「带"呢"语气：…」这类模型自问自答。正确行为是：`content` 为空时
/// 返回 `None`，让上层走「空内容 / 输出被思考吃光」的错误分支（可重试或提示用户），
/// 而不是把思考当译文静默吞下。思考文本只允许进日志（见 [`reasoning_content_len`]）。
fn extract_message_content(value: &serde_json::Value) -> Option<String> {
    let choice = value.get("choices")?.get(0)?;
    let direct = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(content_to_text);
    if direct.is_some() {
        return direct;
    }
    choice
        .get("text")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// 推理模型思考文本（`message.reasoning_content`）的字符数；没有则 0。
///
/// 只用于**诊断与预算判定**：思考文本绝不进入字幕（见 [`extract_message_content`]）。
fn reasoning_content_len(value: &serde_json::Value) -> usize {
    value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|m| m.get("reasoning_content"))
        .and_then(content_to_text)
        .map(|s| s.chars().count())
        .unwrap_or(0)
}

/// 把 `content` 字段还原成文本。
///
/// 兼容两种形态：
/// - 字符串：标准 `chat/completions` 响应；
/// - 数组：部分 OpenAI 兼容网关（以及多模态接口）会把 content 拆成
///   `[{"type":"text","text":"..."}]` 的分片数组。只取字符串的话，这类
///   服务端的译文会被整批判为「空内容」而失败。
fn content_to_text(c: &serde_json::Value) -> Option<String> {
    if let Some(s) = c.as_str() {
        let t = s.trim();
        return if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        };
    }
    let parts = c.as_array()?;
    let joined = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("");
    let t = joined.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cfg() -> OnlineApiConfig {
        OnlineApiConfig {
            endpoint: "https://example.com/v1/chat/completions".to_string(),
            api_key: "sk-test".to_string(),
            model: "test-model".to_string(),
            batch_size: 4,
            timeout_secs: 30,
            glossary_hint: String::new(),
        }
    }

    #[test]
    fn mode_roundtrips_between_str_and_enum() {
        assert_eq!(
            TranslateMode::parse_mode("online_api"),
            TranslateMode::OnlineApi
        );
        assert_eq!(
            TranslateMode::parse_mode("Online"),
            TranslateMode::OnlineApi
        );
        assert_eq!(
            TranslateMode::parse_mode("offline_qwen"),
            TranslateMode::OfflineQwen
        );
        assert_eq!(
            TranslateMode::parse_mode("随便写的"),
            TranslateMode::OfflineQwen
        );
        assert_eq!(TranslateMode::OnlineApi.as_str(), "online_api");
    }

    #[test]
    fn missing_api_key_fails_fast_with_actionable_message() {
        let cfg = OnlineApiConfig {
            api_key: "   ".to_string(),
            ..sample_cfg()
        };
        let segs = vec![Segment::new(1, 0.0, 1.0, "hello")];
        let err = translate_via_online_api(
            &cfg,
            segs,
            "简体中文",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("空密钥必须直接报错而不是静默返回原文");
        assert!(
            err.to_string().contains("API Key"),
            "错误信息应指引用户去填密钥: {err}"
        );
    }

    #[test]
    fn empty_endpoint_fails_fast() {
        let cfg = OnlineApiConfig {
            endpoint: "/chat/completions".to_string(),
            ..sample_cfg()
        };
        let segs = vec![Segment::new(1, 0.0, 1.0, "hello")];
        let err = translate_via_online_api(
            &cfg,
            segs,
            "简体中文",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("空地址必须直接报错");
        assert!(
            err.to_string().contains("接口地址"),
            "错误信息应指向地址配置: {err}"
        );
    }

    #[test]
    fn empty_model_fails_fast() {
        let cfg = OnlineApiConfig {
            model: "  ".to_string(),
            ..sample_cfg()
        };
        let segs = vec![Segment::new(1, 0.0, 1.0, "hello")];
        let err = translate_via_online_api(
            &cfg,
            segs,
            "简体中文",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("空模型名必须报错");
        assert!(err.to_string().contains("模型名"), "{err}");
    }

    #[test]
    fn extracts_content_from_openai_shapes() {
        let standard = serde_json::json!({
            "choices": [{ "message": { "role": "assistant", "content": "[1] 你好。" } }]
        });
        assert_eq!(
            extract_message_content(&standard).as_deref(),
            Some("[1] 你好。")
        );

        let legacy = serde_json::json!({ "choices": [{ "text": "你好" }] });
        assert_eq!(extract_message_content(&legacy).as_deref(), Some("你好"));

        // 推理模型：`reasoning_content` 是**思考过程**，不是译文。
        // 实测（某网关的推理型模型）的思考里混着「或 この内容は…」「带"呢"语气：…」
        // 这类元话语；把它当译文会直接污染字幕。content 为空时必须返回 None，
        // 让上层走「空内容 / 输出被思考吃光」的错误分支。
        let reasoner = serde_json::json!({
            "choices": [{ "message": { "content": "", "reasoning_content": "[1] 你好。" } }]
        });
        assert!(
            extract_message_content(&reasoner).is_none(),
            "思考过程不得当作译文返回"
        );
        // 思考文本只允许用于诊断：长度可读，且**不**受 content 影响
        assert_eq!(reasoning_content_len(&reasoner), 7);
        assert_eq!(reasoning_content_len(&standard), 0);

        let broken = serde_json::json!({ "error": { "message": "boom" } });
        assert!(extract_message_content(&broken).is_none());

        // 部分 OpenAI 兼容网关把 content 拆成分片数组
        let parts = serde_json::json!({
            "choices": [{ "message": { "content": [
                { "type": "text", "text": "[1] 你好。" },
                { "type": "text", "text": "[2] 世界！" }
            ] } }]
        });
        assert_eq!(
            extract_message_content(&parts).as_deref(),
            Some("[1] 你好。[2] 世界！")
        );

        // 空数组 / 无 text 字段 → 视为无内容
        let empty_parts = serde_json::json!({
            "choices": [{ "message": { "content": [] } }]
        });
        assert!(extract_message_content(&empty_parts).is_none());
    }
    /// 重试判定：限流与网络问题是暂时的，参数类错误重试只是浪费用户时间。
    #[test]
    fn retryable_classification() {
        // 值得重试
        assert!(is_retryable(&anyhow!(
            "在线接口返回 HTTP 429（触发限流，请降低批量条数或稍后重试）"
        )));
        assert!(is_retryable(&anyhow!("在线接口返回 HTTP 502: bad gateway")));
        assert!(is_retryable(&anyhow!(
            "无法连接在线翻译接口 https://x/y: timed out"
        )));
        // 不值得重试：密钥/地址/模型名写错，重试多少次都一样
        assert!(!is_retryable(&anyhow!(
            "在线接口返回 HTTP 401（API Key 无效或无权限）: unauthorized"
        )));
        assert!(!is_retryable(&anyhow!(
            "在线接口返回 HTTP 403（API Key 无效或无权限）"
        )));
        assert!(!is_retryable(&anyhow!(
            "在线接口返回 HTTP 404（接口地址或模型名不存在）"
        )));
        assert!(!is_retryable(&anyhow!(
            "在线接口响应不是合法 JSON: expected value"
        )));
        // 4xx 的响应体里提到「HTTP 5xx」时不得被误判为可重试（只看真实状态码）
        assert!(!is_retryable(&anyhow!(
            "在线接口返回 HTTP 400: upstream said HTTP 500, bad request"
        )));
        // 500-系列应重试
        assert!(is_retryable(&anyhow!(
            "在线接口返回 HTTP 503: service unavailable"
        )));
    }

    /// 增量翻译：已有目标语言译文的句子不该再发请求（在线接口按 token 计费）。
    /// 这里验证判定本身——`translation_matches` 是两条链路共用的增量依据。
    #[test]
    fn incremental_skips_segments_already_in_target_language() {
        let mut done = Segment::new(1, 0.0, 1.0, "你好");
        done.translation = Some("Hello".to_string());
        done.translation_lang = Some("English".to_string());

        let mut todo = Segment::new(2, 1.0, 2.0, "世界");
        todo.translation = Some("Bonjour".to_string());
        todo.translation_lang = Some("Français".to_string());

        let segs = [done, todo];
        let pending_en: Vec<usize> = segs
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.translation_matches("English"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(pending_en, vec![1], "已是英文的那句不该重译");

        // 改成日语：两句都要重译
        let pending_ja: Vec<usize> = segs
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.translation_matches("日本語"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(pending_ja, vec![0, 1]);
    }
    // ─────────── 端到端：本地 mock 服务器 ───────────
    //
    // 用真实 TCP + 真实 HTTP 走一遍在线翻译，而不是只测纯函数。
    // 这条链路此前完全依赖手工联调，任何协议/解析/回填的回归都发现不了。

    /// 起一个只服务 N 次请求的最小 HTTP 服务，返回固定的 OpenAI 兼容响应体。
    /// 返回 (地址, 收到的请求体列表)。
    fn spawn_mock_server(replies: Vec<String>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let with_status: Vec<(u16, String)> = replies.into_iter().map(|b| (200, b)).collect();
        spawn_mock_server_status(with_status)
    }

    /// 同上，但可指定每一条响应的 HTTP 状态码（用于验证重试与不可重试分支）。
    fn spawn_mock_server_status(
        replies: Vec<(u16, String)>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定随机端口");
        let addr = format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            listener.local_addr().unwrap().port()
        );
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_clone = seen.clone();

        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                // 读请求头，再按 Content-Length 读满 body
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                let mut content_len = 0usize;
                while let Ok(n) = sock.read(&mut tmp) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        for line in head.lines() {
                            if let Some(v) = line.strip_prefix("content-length:") {
                                content_len = v.trim().parse().unwrap_or(0);
                            }
                        }
                        if buf.len() >= pos + 4 + content_len {
                            break;
                        }
                    }
                }
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let body = String::from_utf8_lossy(&buf[pos + 4..]).to_string();
                    seen_clone.lock().unwrap().push(body);
                }
                let (status, body) = reply;
                let reason = if status == 200 { "OK" } else { "Error" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
        });

        (addr, seen)
    }

    fn online_cfg_for(addr: &str) -> OnlineApiConfig {
        OnlineApiConfig {
            endpoint: addr.to_string(),
            api_key: "sk-test".to_string(),
            model: "mock-model".to_string(),
            batch_size: 20,
            timeout_secs: 10,
            glossary_hint: String::new(),
        }
    }

    /// 完整走一遍在线翻译：请求发出、响应解析、译文回填、语言标记写入。
    #[test]
    fn online_translation_end_to_end_against_mock_server() {
        let reply = serde_json::json!({
            "choices": [{ "message": { "content": "[1] Hello.\n[2] World." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![reply]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![
            Segment::new(1, 0.0, 1.0, "你好"),
            Segment::new(2, 1.0, 2.0, "世界"),
        ];
        let out = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("mock 服务器应返回成功");

        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
        assert_eq!(out[1].translation.as_deref(), Some("World."));
        // 语言标记必须写上，否则下次会被判为「未翻译」而重复请求
        assert_eq!(out[0].translation_lang.as_deref(), Some("English"));
        assert_eq!(out[1].translation_lang.as_deref(), Some("English"));
        assert_eq!(seen.lock().unwrap().len(), 1, "只应发一次请求");
    }

    /// 术语表必须真的注入到发往接口的提示词里（而不是只在本地拼好就丢掉）。
    #[test]
    fn glossary_is_injected_into_request_payload() {
        let reply = serde_json::json!({
            "choices": [{ "message": { "content": "[1] Engine." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![reply]);
        let cfg = OnlineApiConfig {
            glossary_hint: "术语表（以下词条必须按给定译法翻译，不得改写）：生成器=engine。"
                .to_string(),
            ..online_cfg_for(&addr)
        };

        let segs = vec![Segment::new(1, 0.0, 1.0, "生成器")];
        let _ = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("mock 服务器应返回成功");

        let body = seen.lock().unwrap().join("\n");
        assert!(
            body.contains("生成器=engine"),
            "术语表必须出现在请求体里: {body}"
        );
    }

    /// 增量：已有目标语言译文的句子不该再发请求。
    /// 这是在线链路省钱的核心——重复翻译已完成的部分是直接烧 token。
    #[test]
    fn online_translation_is_incremental() {
        let reply = serde_json::json!({
            "choices": [{ "message": { "content": "[2] World." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![reply]);
        let cfg = online_cfg_for(&addr);

        let mut already = Segment::new(1, 0.0, 1.0, "你好");
        already.translation = Some("Hello.".to_string());
        already.translation_lang = Some("English".to_string());

        let todo = Segment::new(2, 1.0, 2.0, "世界");

        let out = translate_via_online_api(
            &cfg,
            vec![already, todo],
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("应成功");

        // 已完成的那句原样保留
        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
        assert_eq!(out[1].translation.as_deref(), Some("World."));
        // 请求体里只应出现序号 2，不含序号 1
        let bodies = seen.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].contains("[2]"), "请求应包含待译的第 2 句");
        assert!(
            !bodies[0].contains("[1]"),
            "已译的第 1 句不应再发给接口: {}",
            bodies[0]
        );
    }

    /// 全部已完成时不发任何请求，直接返回。
    #[test]
    fn online_translation_skips_all_when_complete() {
        let (addr, seen) = spawn_mock_server(vec![]);
        let cfg = online_cfg_for(&addr);

        let mut seg = Segment::new(1, 0.0, 1.0, "你好");
        seg.translation = Some("Hello.".to_string());
        seg.translation_lang = Some("English".to_string());

        let out = translate_via_online_api(
            &cfg,
            vec![seg],
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("应成功");

        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
        assert_eq!(seen.lock().unwrap().len(), 0, "全部完成时不应发请求");
    }
    /// 单批遇到 429 限流要**重试**，而不是让整片翻译前功尽弃。
    /// 第一次回 429、第二次回正常内容 —— 最终应成功，且 mock 确实收到两次请求。
    #[test]
    fn online_translation_retries_on_rate_limit() {
        let ok = serde_json::json!({
            "choices": [{ "message": { "content": "[1] Hello." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server_status(vec![
            (429, r#"{"error":{"message":"rate limited"}}"#.to_string()),
            (200, ok),
        ]);
        let cfg = online_cfg_for(&addr);

        let batch = [Segment::new(1, 0.0, 1.0, "你好")];
        let refs: Vec<&Segment> = batch.iter().collect();
        let map = request_batch_with_retry(
            &cfg,
            &refs,
            "English",
            &AtomicBool::new(false),
            &ModelProfile::default(),
        )
        .expect("429 之后重试应成功");
        assert_eq!(map.get(&1).map(String::as_str), Some("Hello."));
        assert_eq!(seen.lock().unwrap().len(), 2, "429 后必须重发一次请求");
    }

    /// 401（密钥错误）**不该**重试：重试多少次结果都一样，只会让用户多等。
    #[test]
    fn online_translation_does_not_retry_on_auth_error() {
        let (addr, seen) = spawn_mock_server_status(vec![
            (
                401,
                r#"{"error":{"message":"invalid api key"}}"#.to_string(),
            ),
            (
                200,
                r#"{"choices":[{"message":{"content":"[1] 不该走到这里"}}]}"#.to_string(),
            ),
        ]);
        let cfg = online_cfg_for(&addr);

        let batch = [Segment::new(1, 0.0, 1.0, "你好")];
        let refs: Vec<&Segment> = batch.iter().collect();
        let err = request_batch_with_retry(
            &cfg,
            &refs,
            "English",
            &AtomicBool::new(false),
            &ModelProfile::default(),
        )
        .expect_err("401 必须直接失败");
        assert!(err.to_string().contains("401"), "错误信息应带状态码: {err}");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "401 不应重试，只应发一次请求"
        );
    }

    /// 重试次数用尽后必须返回错误（而不是静默成功），
    /// 上层据此跳过该批并继续后面的句子。
    #[test]
    fn online_translation_gives_up_after_max_retries() {
        let (addr, seen) = spawn_mock_server_status(
            (0..MAX_BATCH_RETRIES)
                .map(|_| (429, r#"{"error":{"message":"still limited"}}"#.to_string()))
                .collect(),
        );
        let cfg = online_cfg_for(&addr);

        let batch = [Segment::new(1, 0.0, 1.0, "你好")];
        let refs: Vec<&Segment> = batch.iter().collect();
        let err = request_batch_with_retry(
            &cfg,
            &refs,
            "English",
            &AtomicBool::new(false),
            &ModelProfile::default(),
        )
        .expect_err("重试用尽后应返回错误");
        assert!(err.to_string().contains("429"), "{err}");
        assert_eq!(
            seen.lock().unwrap().len(),
            MAX_BATCH_RETRIES,
            "应恰好重试 MAX_BATCH_RETRIES 次"
        );
    }
    /// `max_tokens` 必须随本批源文本长度增长，且不小于硬下限。
    ///
    /// 在线接口不显式给 `max_tokens` 时部分服务端默认值偏小（512），长批会静默
    /// 截断——返回行数少于请求行数。这个断言锁死「不再依赖服务端默认」。
    #[test]
    fn online_max_tokens_scales_with_batch() {
        let short = [Segment::new(1, 0.0, 1.0, "你好")];
        let short_refs: Vec<&Segment> = short.iter().collect();
        assert_eq!(
            max_output_tokens(&short_refs, 0, false),
            MIN_OUTPUT_TOKENS,
            "极短批也应给足最小生成空间"
        );

        let long_text = "字".repeat(300);
        let long: Vec<Segment> = (1..=20)
            .map(|i| Segment::new(i, 0.0, 1.0, &long_text))
            .collect();
        let long_refs: Vec<&Segment> = long.iter().collect();
        let budget = max_output_tokens(&long_refs, 0, false);
        assert!(
            budget > MIN_OUTPUT_TOKENS,
            "长批的输出预算应显著大于下限，实际 {budget}"
        );
        assert!(budget <= 8_192, "不应超过宽松上限: {budget}");
    }
    /// 在线批次规划：条数上限与字符预算**同时**生效。
    ///
    /// 回归：修前只按条数切，一小批 40 条长字幕拼出的 prompt 可能顶爆小上下文
    /// 服务端并返回 400，而 4xx 被 `is_retryable` 判为不可重试 → 整片翻译在第一批
    /// 就中止。这里锁死「长行会被字符预算提前拆批」，且不得丢句或重复。
    #[test]
    fn plan_online_batches_splits_on_char_budget_and_line_cap() {
        // 短句：应完全受条数上限约束
        let short: Vec<Segment> = (1..=100)
            .map(|i| Segment::new(i, 0.0, 1.0, "短句"))
            .collect();
        let pending: Vec<usize> = (0..100).collect();
        let batches = plan_online_batches(&pending, &short, 20, ONLINE_CHAR_CAP);
        assert!(
            batches.iter().all(|b| b.len() <= 20),
            "每批不得超过条数上限"
        );
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending, "不得丢句或重复");

        // 长句：单批 40 行、每行 500 字 → 字符预算必须把它拆成多批
        let long = "字".repeat(500);
        let segs: Vec<Segment> = (1..=40).map(|i| Segment::new(i, 0.0, 1.0, &long)).collect();
        let pending2: Vec<usize> = (0..40).collect();
        let batches2 = plan_online_batches(&pending2, &segs, 40, ONLINE_CHAR_CAP);
        assert!(batches2.len() > 1, "长行批次应被字符预算拆开");
        for b in &batches2 {
            let chars: usize = b
                .iter()
                .map(|&p| segs[p].translate_source().chars().count() + 6)
                .sum();
            assert!(chars <= ONLINE_CHAR_CAP, "单批字符数应受预算约束: {chars}");
        }
        let mut all2: Vec<usize> = batches2.iter().flatten().copied().collect();
        all2.sort_unstable();
        assert_eq!(all2, pending2, "长句批同样不得丢句或重复");

        // 单条自身超预算：仍必须单独成批，不能死循环
        let huge = vec![Segment::new(1, 0.0, 1.0, "字".repeat(20_000))];
        let b3 = plan_online_batches(&[0], &huge, 40, ONLINE_CHAR_CAP);
        assert_eq!(b3, vec![vec![0]]);

        // 空输入不产出空批次
        assert!(plan_online_batches(&[], &[], 40, ONLINE_CHAR_CAP).is_empty());
    }

    /// 折半重试：第一批返回不完整（缺 [2]），补译请求返回 [2]，最终两句都有译文。
    ///
    /// 修前本批解析不完整只会记一条日志、缺的句子留空；修后应自动补回。
    #[test]
    fn online_retry_fills_missing_lines() {
        let first = serde_json::json!({
            "choices": [{ "message": { "content": "[1] Hello." } }]
        })
        .to_string();
        let second = serde_json::json!({
            "choices": [{ "message": { "content": "[2] World." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![first, second]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![
            Segment::new(1, 0.0, 1.0, "你好"),
            Segment::new(2, 1.0, 2.0, "世界"),
        ];
        let out = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("mock 服务器应返回成功");

        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
        assert_eq!(
            out[1].translation.as_deref(),
            Some("World."),
            "第一批漏掉的 [2] 应被折半重试补回"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "应发出「首批 + 补译」两次请求"
        );
    }

    /// 输出被 `max_tokens` 截断（finish_reason=length）时，末条被切一半的译文不能
    /// 当成完整结果：应丢弃它并交给折半重试补回。这里模拟服务端：第一批
    /// 只返回被截断的 `[1]`（finish_reason=length），重试时返回完整的 `[1]`。
    #[test]
    fn truncated_output_is_not_accepted_as_complete() {
        let truncated = serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": { "content": "[1] Hello, wel" }
            }]
        })
        .to_string();
        let complete = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": { "content": "[1] Hello, welcome." }
            }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![truncated, complete]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![Segment::new(1, 0.0, 1.0, "你好，欢迎。")];
        let out = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("折半重试后应成功");

        assert_eq!(
            out[0].translation.as_deref(),
            Some("Hello, welcome."),
            "被截断的末条不应被接受，应重试拿到完整译文"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "应发出「截断首批 + 补译」两次请求"
        );
    }

    /// `finish_reason` 取值判定：只有 "length" 算截断；"stop" / 缺失 / 异常值都不算。
    #[test]
    fn finish_reason_length_detection() {
        let len = serde_json::json!({ "choices": [{ "finish_reason": "length" }] });
        assert!(finish_reason_is_length(&len));
        let len_upper = serde_json::json!({ "choices": [{ "finish_reason": "LENGTH" }] });
        assert!(finish_reason_is_length(&len_upper));
        let stop = serde_json::json!({ "choices": [{ "finish_reason": "stop" }] });
        assert!(!finish_reason_is_length(&stop));
        let missing = serde_json::json!({ "choices": [{ "message": { "content": "x" } }] });
        assert!(!finish_reason_is_length(&missing));
        let nested = serde_json::json!({
            "choices": [{ "message": { "finish_reason": "length" } }]
        });
        assert!(finish_reason_is_length(&nested));
        assert!(!finish_reason_is_length(&serde_json::json!({})));
    }
    /// 参数类错误（401）不该被「逐批跳过」吞掉：整片会以同一原因全失败，
    /// 最终返回错误，让用户看到真正的失败原因，而不是「完成 0 句」的假成功。
    #[test]
    fn online_translation_fails_fast_on_auth_error() {
        let (addr, _seen) = spawn_mock_server_status(vec![(
            401,
            r#"{"error":{"message":"invalid api key"}}"#.to_string(),
        )]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let err = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("401 应让整片翻译直接失败");

        let msg = err.to_string();
        assert!(msg.contains("401"), "错误信息应带状态码: {msg}");
        assert!(
            msg.contains("在线翻译中止"),
            "应给出可操作的中止提示: {msg}"
        );
    }

    /// 全部待译句都译不出（接口持续 429、重试用尽后逐批跳过）时，应返回错误
    /// 而不是「完成 0 句」的假成功。
    #[test]
    fn online_translation_reports_error_when_nothing_translated() {
        // 每批重试 MAX_BATCH_RETRIES 次；给足响应数以免 mock 耗尽后连接被拒
        let (addr, _seen) = spawn_mock_server_status(
            (0..MAX_BATCH_RETRIES * 4)
                .map(|_| (429, r#"{"error":{"message":"still limited"}}"#.to_string()))
                .collect(),
        );
        let cfg = online_cfg_for(&addr);

        let segs = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let err = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("一句都没译出应报错");

        assert!(
            err.to_string().contains("未能译出任何一句"),
            "应提示没译出任何一句: {err}"
        );
    }

    /// 用户取消不是错误：必须返回**偏序结果**（已译部分），不能报失败。
    ///
    /// 回归：新增的「不可重试错误立刻中止」分支若排在取消判定之前，会把用户主动
    /// 取消（引擎以「翻译已取消」返回）误报成「在线翻译中止」。
    #[test]
    fn online_translation_cancel_returns_partial_not_error() {
        let (addr, _seen) = spawn_mock_server(vec![]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![Segment::new(1, 0.0, 1.0, "你好")];
        // 起手就置位取消：循环第一件事就是 break，返回原始片段（无译文）而非 Err
        let out =
            translate_via_online_api(&cfg, segs, "English", None, Arc::new(AtomicBool::new(true)))
                .expect("取消应返回偏序结果而非错误");
        assert_eq!(out.len(), 1);
        assert!(out[0].translation.is_none(), "取消时不该凭空产生译文");
    }
    // ─────────── P0-A：中文变体短路（zhconv 确定性转换） ───────────

    /// 中文变体目标必须短路到 zhconv，不走在线接口，且写出与 LLM 路径一致的
    /// `translation` / `translation_lang`。
    #[test]
    fn chinese_variant_target_short_circuits_without_request() {
        // mock 服务器准备好一条「绝不会用到」的响应：`seen` 为空即可证明没走网络
        let unused = serde_json::json!({
            "choices": [{ "message": { "content": "[1] 不应出现" } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![unused]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![
            Segment::new(1, 0.0, 1.0, "大家好，下面我们一起来学习概率不等式。"),
            Segment::new(2, 1.0, 2.0, "这个不等式很直观。"),
        ];
        let out = TranslateEngine::online(cfg.clone())
            .translate_subtitles(segs, "繁体中文", None, Arc::new(AtomicBool::new(false)))
            .expect("中文变体应短路成功");

        assert_eq!(seen.lock().unwrap().len(), 0, "中文变体不该发任何网络请求");
        assert!(out
            .iter()
            .all(|s| s.translation_lang.as_deref() == Some("繁体中文")));
        assert!(out.iter().all(|s| s.has_translation()));
        let t0 = out[0].translation.as_deref().unwrap();
        assert_ne!(t0, out[0].translate_source(), "简→繁应改变文本");
        assert!(
            t0.contains('學') || t0.contains('會') || t0.contains('這'),
            "应含繁体字: {t0}"
        );
    }

    /// 例外：目标「简体中文」而源本来就是简体时，转换结果 == 原文。这是**正确行为**，
    /// 必须照常写入译文、并打上语言标记（不能被复制检测当成失败）。
    #[test]
    fn simplified_target_keeps_identity_but_is_still_success() {
        let unused = serde_json::json!({
            "choices": [{ "message": { "content": "[1] 不应出现" } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![unused]);
        let cfg = online_cfg_for(&addr);

        let src = "这个概率不等式很直观。";
        let out = TranslateEngine::online(cfg.clone())
            .translate_subtitles(
                vec![Segment::new(1, 0.0, 1.0, src)],
                "简体中文",
                None,
                Arc::new(AtomicBool::new(false)),
            )
            .expect("简→简应短路成功");

        assert_eq!(seen.lock().unwrap().len(), 0);
        assert_eq!(out[0].translation.as_deref(), Some(src), "简→简应等于原文");
        assert_eq!(out[0].translation_lang.as_deref(), Some("简体中文"));
        assert!(out[0].has_translation(), "等于原文也算已完成，不是失败");
    }

    /// 取消标志必须对中文变体短路同样生效。
    #[test]
    fn chinese_variant_short_circuit_respects_cancel() {
        let unused = serde_json::json!({
            "choices": [{ "message": { "content": "[1] 不应出现" } }]
        })
        .to_string();
        let (addr, _seen) = spawn_mock_server(vec![unused]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![Segment::new(1, 0.0, 1.0, "这是第一句测试文本。")];
        let out = TranslateEngine::online(cfg.clone())
            .translate_subtitles(segs, "繁体中文", None, Arc::new(AtomicBool::new(true)))
            .expect("取消应返回偏序结果");
        assert!(out[0].translation.is_none(), "取消时不该写入译文");
    }

    // ─────────── P0-B：译文 == 原文 复制检测（在线链路） ───────────

    /// 模型把原文原样复制回来时必须判为未译出：走折半重试，重试给出真译文才写回。
    #[test]
    fn online_copy_is_rejected_and_retried() {
        let copy = serde_json::json!({
            "choices": [{ "message": { "content": "[1] 那么这个概率不等式，其实整体里面考的相对来说不是很多。" } }]
        })
        .to_string();
        let real = serde_json::json!({
            "choices": [{ "message": { "content": "[1] This probability inequality is not tested very much overall." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![copy, real]);
        let cfg = online_cfg_for(&addr);

        let src = "那么这个概率不等式，其实整体里面考的相对来说不是很多。";
        let out = translate_via_online_api(
            &cfg,
            vec![Segment::new(1, 0.0, 1.0, src)],
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("折半重试后应拿到真译文");

        assert_eq!(
            out[0].translation.as_deref(),
            Some("This probability inequality is not tested very much overall.")
        );
        assert_eq!(seen.lock().unwrap().len(), 2, "复制应触发一次补译请求");
    }

    /// 短数字 / 纯公式「译文==原文」是正确结果，不能被判为复制而反复重试。
    #[test]
    fn online_short_and_formula_identity_is_accepted() {
        let reply = serde_json::json!({
            "choices": [{ "message": { "content": "[1] 1.3\n[2] P(A+B) = P(A) + P(B) - P(AB)" } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![reply]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![
            Segment::new(1, 0.0, 1.0, "1.3"),
            Segment::new(2, 1.0, 2.0, "P(A+B) = P(A) + P(B) - P(AB)"),
        ];
        let out = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("应成功");

        assert_eq!(out[0].translation.as_deref(), Some("1.3"));
        assert_eq!(
            out[1].translation.as_deref(),
            Some("P(A+B) = P(A) + P(B) - P(AB)")
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "短句/公式不该触发重试");
    }

    /// 整篇复制率过高时必须显式报错，而不是「翻译已完成（N 句）」的假成功。
    #[test]
    fn online_copy_rate_above_threshold_reports_error() {
        let src_lines = [
            "那么这个概率不等式，其实整体里面考的相对来说不是很多。",
            "但是大家平常在做一些习题集的时候会经常遇到。",
            "或者说后面做模拟机员，经常会出现这种类型的题目。",
            "那么关于这不等式的话，很多同学看到就是两眼一麻黑。",
        ];
        let content = src_lines
            .iter()
            .enumerate()
            .map(|(i, t)| format!("[{}] {}", i + 1, t))
            .collect::<Vec<_>>()
            .join("\n");
        let reply = serde_json::json!({
            "choices": [{ "message": { "content": content } }]
        })
        .to_string();
        // 每句都要折半重试（最多 8 轮），给足响应数
        let (addr, _seen) = spawn_mock_server(vec![reply; 40]);
        let cfg = online_cfg_for(&addr);

        let segs: Vec<Segment> = src_lines
            .iter()
            .enumerate()
            .map(|(i, t)| Segment::new(i + 1, i as f64, i as f64 + 1.0, *t))
            .collect();
        let err = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("整篇复制必须报错");

        assert!(
            err.to_string().contains("译文与原文完全相同"),
            "错误信息应说明复制率异常: {err}"
        );
    }

    // ─────────── P0-C：在线预算感知固定开销 ───────────

    /// 在线字符预算必须随固定开销（术语表）单调下降，且不会跌破下限。
    #[test]
    fn online_char_budget_shrinks_with_glossary() {
        let base = online_char_budget(0);
        assert_eq!(base, ONLINE_CHAR_CAP);
        let with_glossary = online_char_budget(2_000);
        assert!(
            with_glossary < base,
            "术语表开销必须压缩在线预算: {with_glossary} vs {base}"
        );
        // 术语表长到超过上限时也不能归零（留最小可用批）
        assert!(online_char_budget(50_000) >= 1_024);
    }

    /// 术语表很长时，在线批次应收缩到「固定开销 + 本批 + 输出」仍能放进上下文。
    #[test]
    fn online_batches_shrink_when_glossary_is_huge() {
        let segs: Vec<Segment> = (1..=60)
            .map(|i| Segment::new(i, 0.0, 1.0, "字".repeat(200)))
            .collect();
        let pending: Vec<usize> = (0..60).collect();

        let no_glossary = plan_online_batches(&pending, &segs, 40, online_char_budget(0));
        let huge = "术语表（以下词条必须按给定译法翻译，不得改写）：".to_string()
            + &"甲=乙；".repeat(1_000);
        let fixed = ONLINE_SYSTEM_PROMPT_CHARS + huge.chars().count() + ONLINE_SRC_HINT_CHARS;
        let budget = online_char_budget(fixed);
        let with_glossary = plan_online_batches(&pending, &segs, 40, budget);

        assert!(
            with_glossary.len() > no_glossary.len(),
            "术语表越长批次数应越多: {} vs {}",
            with_glossary.len(),
            no_glossary.len()
        );
        for b in &with_glossary {
            let chars: usize = b
                .iter()
                .map(|&p| segs[p].translate_source().chars().count() + 6)
                .sum();
            assert!(
                chars <= budget,
                "单批字符数应受预算约束: {chars} > {budget}"
            );
        }
        // 不得丢句或重复
        let mut all: Vec<usize> = with_glossary.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending);
    }

    /// 输出预算也要扣固定开销：术语表越长，`max_tokens` 越小（但仍给足下限）。
    #[test]
    fn online_max_tokens_accounts_for_glossary() {
        let long = "字".repeat(300);
        let segs: Vec<Segment> = (1..=20).map(|i| Segment::new(i, 0.0, 1.0, &long)).collect();
        let refs: Vec<&Segment> = segs.iter().collect();

        let without = max_output_tokens(&refs, 0, false);
        let with = max_output_tokens(&refs, 4_000, false);
        assert!(
            with < without,
            "固定开销应压缩输出预算: {with} vs {without}"
        );
        assert!(with >= MIN_OUTPUT_TOKENS);
    }

    /// P1-E：两套提示词都必须带「数字/公式/变量名/编号/专有名词原样保留」与长度约束，
    /// 且格式说明用引号包裹、无「；」歧义。
    #[test]
    fn prompts_carry_symbol_and_length_constraints() {
        // 在线 prompt 由 request_batch_translation 现场拼装：用一个 mock 服务器抓请求体
        let reply = serde_json::json!({
            "choices": [{ "message": { "content": "[1] Hello." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![reply]);
        let cfg = online_cfg_for(&addr);
        let segs = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let _ = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("应成功");
        let body = seen.lock().unwrap().join("\n");
        assert!(
            body.contains("数字、公式、变量名、编号与专有名词原样保留"),
            "{body}"
        );
        assert!(body.contains("译文长度尽量与原文相当"), "{body}");
        assert!(body.contains("格式严格为「[序号] 译文」"), "{body}");
    }

    // ─────────── P0-1 / P0-2 / P1-3：推理模型（reasoning_content）专项 ───────────

    /// P0-1：`reasoning_content` 是思考过程，**绝不能**回填成译文。
    ///
    /// 回归：修前 `extract_message_content` 在 content 为空时回退到
    /// `reasoning_content`，于是模型的自问自答（「或 …」「带"呢"语气：…」）
    /// 被当成译文写进字幕。这里断言：思考文本既不出现在译文里，也不出现在
    /// 错误信息里；`content` 为空就该失败（上层可重试或提示用户）。
    #[test]
    fn reasoning_content_never_becomes_translation() {
        let thought = "这部分内容呢主要是两块 -> 或 带\"呢\"语气：候选一；候选二。";
        let reply = serde_json::json!({
            "choices": [{ "message": { "content": "", "reasoning_content": thought } }]
        })
        .to_string();
        // 空内容会被「思考吃光预算」分支重试一次，给足两条响应
        let (addr, seen) = spawn_mock_server(vec![reply.clone(), reply]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let err = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("content 为空时必须失败，而不是把思考当译文");

        let msg = err.to_string();
        assert!(
            msg.contains("空内容"),
            "应报「空内容」而不是静默成功: {msg}"
        );
        assert!(
            !msg.contains("候选一"),
            "思考原文不得出现在错误信息里: {msg}"
        );
        // 至少发过一次「加大预算重试」，说明走了推理模型分支
        assert!(
            seen.lock().unwrap().len() >= 2,
            "空内容 + 有思考时应加大预算重试"
        );
    }

    /// P0-2：推理模型的 `max_tokens` 必须给「译文预算 + 思考余量」，而不是 512。
    #[test]
    fn reasoning_budget_adds_thinking_allowance() {
        let short = [Segment::new(1, 0.0, 1.0, "你好")];
        let refs: Vec<&Segment> = short.iter().collect();

        let plain = max_output_tokens(&refs, 0, false);
        let reasoning = max_output_tokens(&refs, 0, true);
        assert_eq!(plain, MIN_OUTPUT_TOKENS, "非推理模型仍是原来的下限");
        assert!(
            reasoning >= REASONING_THINKING_TOKENS,
            "推理模型至少要能装下思考余量: {reasoning}"
        );
        assert!(
            reasoning > plain,
            "推理模型预算必须显著大于非推理模型: {reasoning} vs {plain}"
        );
        assert!(
            reasoning <= MAX_REASONING_OUTPUT_TOKENS,
            "推理模型预算不得超过上限: {reasoning}"
        );

        // 中等长批：译文预算 + 思考余量，不撞上限
        let mid_text = "字".repeat(4_000);
        let mid = [Segment::new(1, 0.0, 1.0, &mid_text)];
        let mid_refs: Vec<&Segment> = mid.iter().collect();
        let mid_budget = max_output_tokens(&mid_refs, 0, true);
        assert!(
            mid_budget > REASONING_THINKING_TOKENS && mid_budget <= MAX_REASONING_OUTPUT_TOKENS,
            "中等长批应叠加思考余量且不超上限: {mid_budget}"
        );

        // 超长批：必须被推理上限截住，不会无限膨胀
        let huge_text = "字".repeat(20_000);
        let huge = [Segment::new(1, 0.0, 1.0, &huge_text)];
        let huge_refs: Vec<&Segment> = huge.iter().collect();
        assert_eq!(
            max_output_tokens(&huge_refs, 0, true),
            MAX_REASONING_OUTPUT_TOKENS
        );
    }

    /// 请求体里的 `max_tokens`。
    fn max_tokens_of(body: &str) -> usize {
        body.split("\"max_tokens\":")
            .nth(1)
            .and_then(|s| s.trim().split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|s| s.parse().ok())
            .expect("请求体里应有 max_tokens")
    }

    /// 请求体 user 消息里出现的 `[N] ` 序号。
    fn requested_indexes(body: &str) -> Vec<usize> {
        body.split("[")
            .skip(1)
            .filter_map(|s| {
                s.split(']')
                    .next()
                    .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|d| d.parse().ok())
            })
            .collect()
    }

    /// P0-2：首批响应暴露推理模型后，**剩余**批次应缩到 [`REASONING_BATCH_LINES`]
    /// 条并改用推理预算；同时必须记录一条日志说明「已调整批次/预算」。
    #[test]
    fn reasoning_model_shrinks_remaining_batches_and_raises_budget() {
        // 首批 8 条，顺带带回思考文本 → 判定推理模型
        let first = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "[1] T1.\n[2] T2.\n[3] T3.\n[4] T4.\n[5] T5.\n[6] T6.\n[7] T7.\n[8] T8.",
                    "reasoning_content": "先想想怎么翻这八句……"
                }
            }]
        })
        .to_string();
        let six_a = serde_json::json!({
            "choices": [{ "message": { "content": "[9] T9.\n[10] T10.\n[11] T11.\n[12] T12.\n[13] T13.\n[14] T14." } }]
        })
        .to_string();
        let six_b = serde_json::json!({
            "choices": [{ "message": { "content": "[15] T15.\n[16] T16.\n[17] T17.\n[18] T18.\n[19] T19.\n[20] T20." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![first, six_a, six_b]);
        // 首批限 8 条：首批响应暴露推理模型后，剩余 12 条应按 6 条/批重排。
        let cfg = OnlineApiConfig {
            batch_size: 8,
            ..online_cfg_for(&addr)
        };

        let segs: Vec<Segment> = (1..=20)
            .map(|i| Segment::new(i, 0.0, 1.0, "短句"))
            .collect();
        let out = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("推理模型应能译完");

        assert_eq!(out.len(), 20);
        assert!(out.iter().all(|s| s.has_translation()), "20 句都应译出");

        let bodies = seen.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            3,
            "首批 8 条 + 剩余 12 条按 6 条/批 = 共 3 次请求"
        );
        // 剩余批必须被缩到 REASONING_BATCH_LINES 条（而不是继续按 8 条切）
        for body in &bodies[1..] {
            let idx = requested_indexes(body);
            assert!(
                idx.len() <= REASONING_BATCH_LINES,
                "识别出推理模型后每批不得超过 {} 条，实际 {:?}",
                REASONING_BATCH_LINES,
                idx
            );
            assert!(
                max_tokens_of(body) >= REASONING_THINKING_TOKENS,
                "剩余批必须改用推理预算: {}",
                max_tokens_of(body)
            );
        }
        // 不得重发已译句子
        let later: Vec<usize> = bodies[1..]
            .iter()
            .flat_map(|b| requested_indexes(b))
            .collect();
        assert!(
            later.iter().all(|i| *i >= 9),
            "已译的第 1~8 句不得重发: {later:?}"
        );
    }

    /// P0-2 方案 C：首批只回思考、没回译文时，应**加大预算重试**，而不是把思考
    /// 当译文或直接判失败。
    #[test]
    fn reasoning_model_retries_with_bigger_budget_when_thought_eats_output() {
        let thought_only = serde_json::json!({
            "choices": [{ "message": { "content": "", "reasoning_content": "思考中……" } }]
        })
        .to_string();
        let ok = serde_json::json!({
            "choices": [{ "message": { "content": "[1] Hello.\n[2] World." } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![thought_only, ok]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![
            Segment::new(1, 0.0, 1.0, "你好"),
            Segment::new(2, 1.0, 2.0, "世界"),
        ];
        let out = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("加大预算重试后应拿到译文");

        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
        assert_eq!(out[1].translation.as_deref(), Some("World."));
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "应发出「失败首批 + 加大预算重试」"
        );

        let bodies = seen.lock().unwrap().clone();
        let mt: usize = bodies[1]
            .split("\"max_tokens\":")
            .nth(1)
            .and_then(|s| s.trim().split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|s| s.parse().ok())
            .expect("重试请求体里应有 max_tokens");
        assert!(
            mt >= REASONING_THINKING_TOKENS,
            "重试必须给足推理预算: {mt}"
        );
    }
}
