//! 字幕翻译引擎 — 支持离线 Qwen 大模型字幕翻译及在线 API 极速翻译
//!
//! 两条链路共用同一套「逐行 `[序号] 译文`」协议与解析器：
//! - `OfflineQwen`：本地 llama.cpp + Qwen 小模型，免费、断网可用、无需密钥；
//! - `OnlineApi`：任意 OpenAI 兼容的 `/chat/completions` 接口
//!   （DeepSeek / OpenAI / 通义 / Kimi / 本地 vLLM / Ollama 均可）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::{info, warn};

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

    pub fn from_str(s: &str) -> Self {
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
        progress_cb: Option<Box<dyn Fn(f64, &str) + Send>>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<Segment>> {
        info!("开始执行字幕翻译 (目标语言: {}, 引擎: {:?})", target_lang, self.mode);
        match self.mode {
            TranslateMode::OfflineQwen => {
                let engine = self
                    .llm_engine
                    .as_ref()
                    .ok_or_else(|| anyhow!("离线翻译引擎未初始化"))?;
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
    progress_cb: Option<Box<dyn Fn(f64, &str) + Send>>,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<Segment>> {
    if cfg.api_key.trim().is_empty() {
        return Err(anyhow!(
            "未配置在线翻译 API Key。请在「性能设置 → 在线翻译 API」中填写，\
             或设置环境变量 VOICE2WORD_API_KEY"
        ));
    }
    if cfg.model.trim().is_empty() {
        return Err(anyhow!("未配置在线翻译模型名（如 deepseek-chat / gpt-4o-mini）"));
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

    for chunk in plan_online_batches(&translatable, &segments, batch_size) {
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
        let translations = match request_batch_with_retry(cfg, &batch, target_lang, &cancel) {
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
        let mut matched = 0usize;
        for &pos in &chunk {
            if let Some(text) = translations.get(&segments[pos].index) {
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
            let recovered = retry_missing_online(
                cfg,
                &mut segments,
                &missing,
                target_lang,
                &cancel,
            );
            if recovered < missing.len() {
                soft_failed_batches += 1;
                warn!(
                    "在线翻译本批输出不完整（{matched}/{} 条，折半重试后补回 {recovered} 条）",
                    chunk.len()
                );
            }
        }
        completed += chunk.len();
        if let Some(ref cb) = progress_cb {
            let progress = completed as f64 / pending_total.max(1) as f64;
            cb(
                progress,
                &format!("在线翻译中: {completed}/{pending_total} 条（{}）", cfg.model),
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
        if ok == 0 {
            return Err(anyhow!(
                "在线翻译未能译出任何一句（共 {pending_total} 句待译）。请用「性能设置 → 在线翻译 API → 测试连接」检查地址、密钥与模型名。"
            ));
        }
    }
    if soft_failed_batches > 0 {
        info!("在线 API 字幕翻译完成（{soft_failed_batches} 批未成功，可再次点击「开始翻译」补译）");
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
            match request_batch_with_retry(cfg, &batch, target_lang, cancel) {
                Ok(map) => {
                    for &pos in sub {
                        if let Some(text) = map.get(&segments[pos].index) {
                            segments[pos].translation = Some(text.clone());
                            segments[pos].translation_lang = Some(target_lang.to_string());
                            recovered += 1;
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
) -> Result<HashMap<usize, String>> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..MAX_BATCH_RETRIES {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("翻译已取消"));
        }
        match request_batch_translation(cfg, batch, target_lang) {
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
/// 这里复用同一套字符预算思路：单批源文本（含 `[序号] ` 前缀）超过 [`ONLINE_CHAR_BUDGET`]
/// 就提前收口，单条自身超预算时仍单独成批（交回服务端处理总好过在这里死循环）。
fn plan_online_batches(pending: &[usize], segments: &[Segment], max_lines: usize) -> Vec<Vec<usize>> {
    let max_lines = max_lines.max(1);
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_chars = 0usize;
    for &pos in pending {
        // 序号文本（"[123] "）也算输入，粗估 6 字符
        let cost = segments[pos].translate_source().chars().count() + 6;
        let would_overflow = !current.is_empty()
            && (current.len() >= max_lines || current_chars + cost > ONLINE_CHAR_BUDGET);
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

/// 在线链路单批的**源文本字符预算**（含 `[序号] ` 前缀）。
///
/// 推导：按最保守的 8K token 上下文估算——中文约 1 token/字符，一批要同时装下
/// 「输入 prompt」与「模型输出」，再给系统提示词留 ~200 token，于是单批源文本
/// 与译文各约 (8192 - 200) / 2 ≈ 4000 字符；再压到 3500 留安全余量。
///
/// 注意这只是**安全上限**：普通字幕（每行 < 60 字）即便 40 行也才 ~2400 字符，
/// 根本够不到预算，因此常规批次仍由条数上限（20/40 条）决定，不会变慢；
/// 只有出现**超长行**时预算才会提前收口——那正是顶爆小上下文服务端的场景。
const ONLINE_CHAR_BUDGET: usize = 3_500;

/// 一条字幕译文大致需要的输出 token（本地 Qwen2.5 实测中→英约 0.84 token/字符，
/// 按 1.0 封顶给膨胀型目标语言留余量）。
const OUTPUT_TOKENS_PER_CHAR: usize = 1;
/// 输出 token 的硬下限。
///
/// 不能只算“译文本身需要多少 token”——很多在线模型是**推理模型**，
/// 会先花几百 token “思考”再输出译文，而这些思考 token 也占 `max_tokens`。
/// 下限太低时，模型思考到一半就被截断，译文根本没开始写。
/// 实测（deepseek-v4-flash，三条短字幕）：思考占 ~184 token、译文 ~50 token。
/// 取 512 作为下限，给推理模型留出足够的思考空间。
const MIN_OUTPUT_TOKENS: usize = 512;

/// 输出 token 的宽松上限：防个别服务对超大值报错。
const MAX_OUTPUT_TOKENS: usize = 8_192;

/// 按本批源文本长度推导 `max_tokens`。
///
/// 在线接口不显式给 `max_tokens` 时，部分服务端默认值偏小（如 512），长批会**静默
/// 截断**——返回的译文行数少于请求行数，而 `parse_batch_response` 只认能解析出的行，
/// 少掉的那几句就原样留空。这里按源字符数给足预算（不再依赖服务端默认），
/// 同时用一个宽松上限防止个别服务对超大值报错。
fn max_output_tokens(batch: &[&Segment]) -> usize {
    let chars: usize = batch
        .iter()
        .map(|seg| seg.translate_source().chars().count())
        .sum();
    (chars * OUTPUT_TOKENS_PER_CHAR).clamp(MIN_OUTPUT_TOKENS, MAX_OUTPUT_TOKENS)
}

/// 请求一批字幕的译文，返回 `序号 -> 译文` 映射
fn request_batch_translation(
    cfg: &OnlineApiConfig,
    batch: &[&Segment],
    target_lang: &str,
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
                    "你是专业字幕翻译专家。{src_hint}{glossary}把用户给出的带序号字幕逐条翻译为地道的{target_lang}，\
                     保持原意与语气，语言通顺紧凑。\
                     必须逐行输出，格式严格为「[序号] 译文」，不得解释、不得合并、不得遗漏、不得改动序号。",
                    glossary = cfg.glossary_hint
                )
            },
            { "role": "user", "content": source }
        ],
        "temperature": 0.2,
        "max_tokens": max_output_tokens(batch),
        "stream": false,
    });

    let value = post_chat_completions(cfg, &payload)?;
    let content = extract_message_content(&value).unwrap_or_default();
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
    reason.map(|r| r.eq_ignore_ascii_case("length")).unwrap_or(false)
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
        Err(e) => Err(anyhow!(
            "无法连接在线翻译接口 {}: {e}",
            cfg.endpoint
        )),
    }
}

/// 从 OpenAI 兼容响应里取出回复文本。
/// 兼容三种形态：`chat/completions` 的 `message.content`、
/// 推理模型的 `message.reasoning_content`（content 为空时的兜底）、
/// 以及旧版 `completions` 的 `choices[0].text`。
fn extract_message_content(value: &serde_json::Value) -> Option<String> {
    let choice = value.get("choices")?.get(0)?;
    let direct = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(content_to_text);
    if direct.is_some() {
        return direct;
    }
    let legacy = choice
        .get("text")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    if legacy.is_some() {
        return legacy;
    }
    choice
        .get("message")
        .and_then(|m| m.get("reasoning_content"))
        .and_then(content_to_text)
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
        return if t.is_empty() { None } else { Some(t.to_string()) };
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
        assert_eq!(TranslateMode::from_str("online_api"), TranslateMode::OnlineApi);
        assert_eq!(TranslateMode::from_str("Online"), TranslateMode::OnlineApi);
        assert_eq!(TranslateMode::from_str("offline_qwen"), TranslateMode::OfflineQwen);
        assert_eq!(TranslateMode::from_str("随便写的"), TranslateMode::OfflineQwen);
        assert_eq!(TranslateMode::OnlineApi.as_str(), "online_api");
    }

    #[test]
    fn missing_api_key_fails_fast_with_actionable_message() {
        let cfg = OnlineApiConfig {
            api_key: "   ".to_string(),
            ..sample_cfg()
        };
        let segs = vec![Segment::new(1, 0.0, 1.0, "hello")];
        let err = translate_via_online_api(&cfg, segs, "简体中文", None, Arc::new(AtomicBool::new(false)))
            .expect_err("空密钥必须直接报错而不是静默返回原文");
        assert!(err.to_string().contains("API Key"), "错误信息应指引用户去填密钥: {err}");
    }

    #[test]
    fn empty_endpoint_fails_fast() {
        let cfg = OnlineApiConfig {
            endpoint: "/chat/completions".to_string(),
            ..sample_cfg()
        };
        let segs = vec![Segment::new(1, 0.0, 1.0, "hello")];
        let err =
            translate_via_online_api(&cfg, segs, "简体中文", None, Arc::new(AtomicBool::new(false)))
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
        let err = translate_via_online_api(&cfg, segs, "简体中文", None, Arc::new(AtomicBool::new(false)))
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

        // 推理模型：content 为空时退回 reasoning_content，而不是判定失败
        let reasoner = serde_json::json!({
            "choices": [{ "message": { "content": "", "reasoning_content": "[1] 你好。" } }]
        });
        assert_eq!(
            extract_message_content(&reasoner).as_deref(),
            Some("[1] 你好。")
        );

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
        assert!(is_retryable(&anyhow!("在线接口返回 HTTP 429（触发限流，请降低批量条数或稍后重试）")));
        assert!(is_retryable(&anyhow!("在线接口返回 HTTP 502: bad gateway")));
        assert!(is_retryable(&anyhow!("无法连接在线翻译接口 https://x/y: timed out")));
        // 不值得重试：密钥/地址/模型名写错，重试多少次都一样
        assert!(!is_retryable(&anyhow!("在线接口返回 HTTP 401（API Key 无效或无权限）: unauthorized")));
        assert!(!is_retryable(&anyhow!("在线接口返回 HTTP 403（API Key 无效或无权限）")));
        assert!(!is_retryable(&anyhow!("在线接口返回 HTTP 404（接口地址或模型名不存在）")));
        assert!(!is_retryable(&anyhow!("在线接口响应不是合法 JSON: expected value")));
        // 4xx 的响应体里提到「HTTP 5xx」时不得被误判为可重试（只看真实状态码）
        assert!(!is_retryable(&anyhow!(
            "在线接口返回 HTTP 400: upstream said HTTP 500, bad request"
        )));
        // 500-系列应重试
        assert!(is_retryable(&anyhow!("在线接口返回 HTTP 503: service unavailable")));
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

        let segs = vec![done, todo];
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
    fn spawn_mock_server(
        replies: Vec<String>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
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
        let addr = format!("http://127.0.0.1:{}/v1/chat/completions", listener.local_addr().unwrap().port());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_clone = seen.clone();

        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept() else { return };
                // 读请求头，再按 Content-Length 读满 body
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                let mut content_len = 0usize;
                loop {
                    let Ok(n) = sock.read(&mut tmp) else { break };
                    if n == 0 { break }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        for line in head.lines() {
                            if let Some(v) = line.strip_prefix("content-length:") {
                                content_len = v.trim().parse().unwrap_or(0);
                            }
                        }
                        if buf.len() >= pos + 4 + content_len { break }
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
            glossary_hint: "术语表（以下词条必须按给定译法翻译，不得改写）：生成器=engine。".to_string(),
            ..online_cfg_for(&addr)
        };

        let segs = vec![Segment::new(1, 0.0, 1.0, "生成器")];
        let _ = translate_via_online_api(&cfg, segs, "English", None, Arc::new(AtomicBool::new(false)))
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
        assert!(!bodies[0].contains("[1]"), "已译的第 1 句不应再发给接口: {}", bodies[0]);
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

        let batch = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let refs: Vec<&Segment> = batch.iter().collect();
        let map = request_batch_with_retry(&cfg, &refs, "English", &AtomicBool::new(false))
            .expect("429 之后重试应成功");
        assert_eq!(map.get(&1).map(String::as_str), Some("Hello."));
        assert_eq!(seen.lock().unwrap().len(), 2, "429 后必须重发一次请求");
    }

    /// 401（密钥错误）**不该**重试：重试多少次结果都一样，只会让用户多等。
    #[test]
    fn online_translation_does_not_retry_on_auth_error() {
        let (addr, seen) = spawn_mock_server_status(vec![
            (401, r#"{"error":{"message":"invalid api key"}}"#.to_string()),
            (200, r#"{"choices":[{"message":{"content":"[1] 不该走到这里"}}]}"#.to_string()),
        ]);
        let cfg = online_cfg_for(&addr);

        let batch = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let refs: Vec<&Segment> = batch.iter().collect();
        let err = request_batch_with_retry(&cfg, &refs, "English", &AtomicBool::new(false))
            .expect_err("401 必须直接失败");
        assert!(err.to_string().contains("401"), "错误信息应带状态码: {err}");
        assert_eq!(seen.lock().unwrap().len(), 1, "401 不应重试，只应发一次请求");
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

        let batch = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let refs: Vec<&Segment> = batch.iter().collect();
        let err = request_batch_with_retry(&cfg, &refs, "English", &AtomicBool::new(false))
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
        let short = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let short_refs: Vec<&Segment> = short.iter().collect();
        assert_eq!(
            max_output_tokens(&short_refs),
            MIN_OUTPUT_TOKENS,
            "极短批也应给足最小生成空间"
        );

        let long_text = "字".repeat(300);
        let long: Vec<Segment> = (1..=20).map(|i| Segment::new(i, 0.0, 1.0, &long_text)).collect();
        let long_refs: Vec<&Segment> = long.iter().collect();
        let budget = max_output_tokens(&long_refs);
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
        let short: Vec<Segment> = (1..=100).map(|i| Segment::new(i, 0.0, 1.0, "短句")).collect();
        let pending: Vec<usize> = (0..100).collect();
        let batches = plan_online_batches(&pending, &short, 20);
        assert!(batches.iter().all(|b| b.len() <= 20), "每批不得超过条数上限");
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending, "不得丢句或重复");

        // 长句：单批 40 行、每行 500 字 → 字符预算必须把它拆成多批
        let long = "字".repeat(500);
        let segs: Vec<Segment> = (1..=40).map(|i| Segment::new(i, 0.0, 1.0, &long)).collect();
        let pending2: Vec<usize> = (0..40).collect();
        let batches2 = plan_online_batches(&pending2, &segs, 40);
        assert!(batches2.len() > 1, "长行批次应被字符预算拆开");
        for b in &batches2 {
            let chars: usize = b
                .iter()
                .map(|&p| segs[p].translate_source().chars().count() + 6)
                .sum();
            assert!(chars <= ONLINE_CHAR_BUDGET, "单批字符数应受预算约束: {chars}");
        }
        let mut all2: Vec<usize> = batches2.iter().flatten().copied().collect();
        all2.sort_unstable();
        assert_eq!(all2, pending2, "长句批同样不得丢句或重复");

        // 单条自身超预算：仍必须单独成批，不能死循环
        let huge = vec![Segment::new(1, 0.0, 1.0, "字".repeat(20_000))];
        let b3 = plan_online_batches(&[0], &huge, 40);
        assert_eq!(b3, vec![vec![0]]);

        // 空输入不产出空批次
        assert!(plan_online_batches(&[], &[], 40).is_empty());
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
        assert_eq!(seen.lock().unwrap().len(), 2, "应发出「首批 + 补译」两次请求");
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
        assert_eq!(seen.lock().unwrap().len(), 2, "应发出「截断首批 + 补译」两次请求");
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
        let (addr, _seen) = spawn_mock_server_status(vec![
            (401, r#"{"error":{"message":"invalid api key"}}"#.to_string()),
        ]);
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
        assert!(msg.contains("在线翻译中止"), "应给出可操作的中止提示: {msg}");
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
        let out = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(true)),
        )
        .expect("取消应返回偏序结果而非错误");
        assert_eq!(out.len(), 1);
        assert!(out[0].translation.is_none(), "取消时不该凭空产生译文");
    }
}
