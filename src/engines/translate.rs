//! 字幕翻译引擎 — 支持离线 Qwen 大模型字幕翻译及在线 API 极速翻译
//!
//! 两条链路共用同一套「逐行 `[序号] 译文`」协议与解析器：
//! - `OfflineQwen`：本地 llama.cpp + Qwen 小模型，免费、断网可用、无需密钥；
//! - `OnlineApi`：任意 OpenAI 兼容的 `/chat/completions` 接口
//!   （DeepSeek / OpenAI / 通义 / Kimi / 本地 vLLM / Ollama 均可）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::{debug, info, warn};

use crate::engines::llm::{
    chinese_variant_target, convert_chinese_variant, validate_translation, CopyRate, RewriteStats,
    RewriteVerdict, REFLECT_PROMPT,
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
#[derive(Clone)]
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

/// `api_key` 的日志包装：`{:?}` 只暴露前 4 位，其余打码。
///
/// 为什么不能直接 `#[derive(Debug)]`：`api_key` 是**明文密钥字段**，任何一处后来
/// 加上的 `debug!(?cfg)` / `info!("{cfg:?}")` 都会把密钥整条写进日志文件，
/// 而用户往往会把日志贴进工单或聊天窗口。密钥一旦离开本机就只能吊销重发。
/// 因此把「打码」塞进 `Debug` 实现本身，而不是指望每个调用点自觉脱敏。
///
/// 前 4 位保留是为了排障时能看出「填的是哪把 key」（`sk-…` 还是别家的），
/// 但对短密钥（≤8 字符）连前 4 位也不给：短串的前 4 位几乎泄露一半。
struct RedactedKey<'a>(&'a str);

impl std::fmt::Debug for RedactedKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = self.0;
        if key.trim().is_empty() {
            return f.write_str("<empty>");
        }
        if key.chars().count() > 8 {
            let head: String = key.chars().take(4).collect();
            return write!(f, "{head}…<redacted>");
        }
        f.write_str("<redacted>")
    }
}

/// 手写 `Debug`：`api_key` 走 [`RedactedKey`] 打码，其余字段照常输出。
///
/// 不能 `derive`：derive 会把 `pub api_key` 原文打进任何 `{:?}`。
impl std::fmt::Debug for OnlineApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnlineApiConfig")
            .field("endpoint", &self.endpoint)
            .field("api_key", &RedactedKey(&self.api_key))
            .field("model", &self.model)
            .field("batch_size", &self.batch_size)
            .field("timeout_secs", &self.timeout_secs)
            .field("glossary_hint", &self.glossary_hint)
            .finish()
    }
}

/// 部分结果回传槽：在线链路在**整片中终止**前把已写回的译文放进这里。
///
/// 在线翻译按 token 计费，一次任务可能已经跑完好几十批。整片中止（配置错误、
/// 质量异常）时若不回传这部分结果，一次故障就会让本次已算好、已计费的部分全部作废。
/// 界面在失败分支读它并合并回字幕表（见 `ui::actions`）。
pub type PartialSink = Arc<Mutex<Option<Vec<Segment>>>>;

/// 把当前进度下的译文存进部分结果槽（供失败分支合并）。
///
/// 只做**克隆**：调用点上往往还借着 `segments`（例如持有 `batch: Vec<&Segment>`），
/// 移动所有权会与借用冲突；错误路径本就罕见，拷贝几百条 `Segment` 的代价可忽略。
fn stash_partial(sink: Option<&PartialSink>, segments: &[Segment]) {
    if let Some(sink) = sink {
        *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(segments.to_vec());
    }
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
    /// 最近一次翻译的「输出校验自愈」统计（回退条数 / 反思次数），供上层提示。
    /// 离线链路从 [`LLMEngine::last_rewrite_stats`] 抄回，在线链路由
    /// `translate_via_online_api_with_stats` 直接给出；用 `Mutex` 是因为引擎挂在
    /// `Arc` 后面共享、写回时拿不到 `&mut self`。
    rewrite_stats: Mutex<RewriteStats>,
    /// 在线链路整片中终止时的部分结果回传槽；未设置时为 `None`（离线链路不使用）。
    partial_sink: Option<PartialSink>,
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
            rewrite_stats: Mutex::new(RewriteStats::default()),
            partial_sink: None,
        }
    }

    pub fn online(config: OnlineApiConfig) -> Self {
        Self {
            mode: TranslateMode::OnlineApi,
            llm_engine: None,
            online: Some(config),
            rewrite_stats: Mutex::new(RewriteStats::default()),
            partial_sink: None,
        }
    }

    pub fn mode(&self) -> TranslateMode {
        self.mode
    }

    /// 最近一次翻译中「输出校验自愈」的统计（供上层提示用户）。
    pub fn last_rewrite_stats(&self) -> RewriteStats {
        *self.rewrite_stats.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 写回本次翻译的校验自愈统计。
    fn store_rewrite_stats(&self, stats: RewriteStats) {
        *self.rewrite_stats.lock().unwrap_or_else(|e| e.into_inner()) = stats;
    }

    /// 设置部分结果回传槽：在线链路整片中终止前会把已译好的部分存进这里，
    /// 让界面失败时仍能合并已计费的结果（见 [`PartialSink`] 与 `ui::actions`）。
    pub fn set_partial_sink(&mut self, sink: PartialSink) {
        self.partial_sink = Some(sink);
    }

    /// 批量翻译字幕片段为目标语言（默认 "简体中文"）
    pub fn translate_subtitles(
        &self,
        segments: Vec<Segment>,
        target_lang: &str,
        progress_cb: Option<crate::engines::TextProgressCb>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<Segment>> {
        // 目标语言来自 CLI（`--translate`）、`config.toml` 与界面，可能带首尾空白。
        // 必须在这里归一化：带空白的语言名会被原样写进 `Segment::translation_lang`，
        // 而界面收尾统计与 `Segment::translation_matches` 都是**精确比对**，
        // 「 English」与「English」对不上，于是界面永远显示「未译」，每点一次
        // 「开始翻译」都把整片重译一遍（在线链路还要重复计费）。
        // 空值同样拦下：提示词会退化成「翻译为地道的。」，写回的
        // `translation_lang = Some("")` 还会让 `translation_matches("")` 恒真——
        // 变成「界面说已全部译完、实际并没有目标语言」的假成功。
        let target_lang = ensure_target_lang(target_lang)?;
        info!(
            "开始执行字幕翻译 (目标语言: {}, 引擎: {:?})",
            target_lang, self.mode
        );
        // P0-A：目标语言是**中文变体**且源语言也是中文时短路走 zhconv 本地确定性转换，不走 LLM。
        //
        // 关键前提：源字幕必须本身已经是中文！
        // 若源字幕是日语（含假名）、韩语（含谚文）、英语（拉丁文）等外语，目标语言为中文时
        // 属于真正的跨语言翻译，绝不能短路到 zhconv（zhconv 对假名与外语毫无反应，
        // 会把外语原文原封不动吐回，导致 0 毫秒假完成且完全没有翻译）。
        if crate::subtitle::is_source_chinese(&segments) {
            if let Some(variant) = chinese_variant_target(target_lang) {
                self.store_rewrite_stats(RewriteStats::default());
                return convert_chinese_variant(segments, target_lang, variant, progress_cb, &cancel);
            }
        }
        match self.mode {
            TranslateMode::OfflineQwen => {
                let engine = self
                    .llm_engine
                    .as_ref()
                    .ok_or_else(|| anyhow!("离线翻译引擎未初始化"))?;
                // P0-A（离线侧）：源为中文且目标是中文变体时同样短路
                if crate::subtitle::is_source_chinese(&segments) {
                    if let Some(variant) = chinese_variant_target(target_lang) {
                        self.store_rewrite_stats(RewriteStats::default());
                        return convert_chinese_variant(
                            segments,
                            target_lang,
                            variant,
                            progress_cb,
                            &cancel,
                        );
                    }
                }
                // 取消标志下沉到引擎：润色/翻译都是分批长循环，取消必须能在批次之间生效
                engine.install_cancel_flag(cancel);
                let result = engine.translate(segments, target_lang, progress_cb);
                // 离线链路的自愈统计在引擎内累积，这里读出来统一暴露给上层。
                self.store_rewrite_stats(engine.last_rewrite_stats());
                result
            }
            TranslateMode::OnlineApi => {
                let cfg = self
                    .online
                    .as_ref()
                    .ok_or_else(|| anyhow!("在线翻译接口未配置"))?;
                let (translated, stats) = translate_via_online_api_with_stats(
                    cfg,
                    segments,
                    target_lang,
                    progress_cb,
                    cancel,
                    self.partial_sink.as_ref(),
                )?;
                self.store_rewrite_stats(stats);
                Ok(translated)
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
        let mut payload = serde_json::json!({
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
        if !supports_temperature(&cfg.model) {
            if let Some(obj) = payload.as_object_mut() {
                obj.remove("temperature");
            }
        }
        let value = post_chat_completions(cfg, &payload)?;
        Ok(extract_message_content(&value)
            .unwrap_or_else(|| "(接口返回成功，但未解析到文本内容)".to_string()))
    }
}

/// 归一化并校验翻译目标语言，返回去掉首尾空白的语言名。
///
/// # 为什么必须归一化
///
/// `Segment::translation_lang` 与 `Segment::translation_matches` 都是**精确比对**，
/// 且增量翻译、收尾统计、界面按钮三处共用同一套判定。目标语言若带首尾空白
/// （CLI 参数、手改的 `config.toml` 都可能带来），写出的标签就是「 English」，
/// 与界面上的「English」对不上：界面显示「未译」，用户再点「开始翻译」又整片重译。
///
/// # 为什么空值必须拒绝
///
/// 空串/纯空白会让提示词退化成「将用户给出的带序号字幕逐条翻译为地道的。」，
/// 而回填写下的 `translation_lang = Some("")` 会让 `translation_matches("")` 恒真，
/// 此后再也不会重译——「界面说全部译完、实际并没有目标语言」的假成功。
/// 宁可在这里明确报错，也不要静默产出一份没有目标语言的译文。
fn ensure_target_lang(target_lang: &str) -> Result<&str> {
    let trimmed = target_lang.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "目标语言为空：请先选择要翻译成的语言（如 简体中文 / 繁体中文 / English）"
        ));
    }
    Ok(trimmed)
}

/// 在线翻译主流程的**测试兼容入口**：保留原签名，让既有回归测试不必改写；自愈统计
/// 由 [`translate_via_online_api_with_stats`] 返回，在这里丢弃。
/// 生产链路（[`TranslateEngine::translate_subtitles`]）直接调用带统计的版本。
#[cfg(test)]
fn translate_via_online_api(
    cfg: &OnlineApiConfig,
    segments: Vec<Segment>,
    target_lang: &str,
    progress_cb: Option<crate::engines::TextProgressCb>,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<Segment>> {
    translate_via_online_api_with_stats(cfg, segments, target_lang, progress_cb, cancel, None)
        .map(|(segments, _stats)| segments)
}

/// 在线翻译主流程：按 `batch_size` 切片，逐批请求并回填 `segment.translation`，
/// 并返回「输出校验自愈」统计（回退条数 / 反思次数）供上层提示。
fn is_local_endpoint(endpoint: &str) -> bool {
    let lower = endpoint.to_lowercase();
    lower.contains("localhost")
        || lower.contains("127.0.0.1")
        || lower.contains("0.0.0.0")
        || lower.contains("192.168.")
        || lower.contains("10.")
        || lower.contains(":11434")
}

/// 部分前沿推理模型（如 OpenAI o1 / o3-mini 等）严格禁止自定义 temperature 参数，
/// 传入非默认值会直接报 HTTP 400 Bad Request。
fn supports_temperature(model: &str) -> bool {
    let m = model.to_lowercase();
    !(m.starts_with("o1") || m.starts_with("o3") || m.contains("-o1") || m.contains("-o3"))
}

fn translate_via_online_api_with_stats(
    cfg: &OnlineApiConfig,
    mut segments: Vec<Segment>,
    target_lang: &str,
    progress_cb: Option<crate::engines::TextProgressCb>,
    cancel: Arc<AtomicBool>,
    partial: Option<&PartialSink>,
) -> Result<(Vec<Segment>, RewriteStats)> {
    if cfg.api_key.trim().is_empty() && !is_local_endpoint(&cfg.endpoint) {
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
        return Ok((segments, RewriteStats::default()));
    }
    if pending_total < total {
        info!("在线增量翻译：{total} 句中有 {pending_total} 句需要翻译为 {target_lang}");
    }

    let mut completed = 0usize;
    // 失败但**不需要中止整片**的批次数（重试用尽、或本批解析不完整）
    let mut soft_failed_batches = 0usize;
    // 最近一次软失败的原因：全部批次都失败时并进最终错误信息，
    // 免得把真正的根因（如推理模型的思考吃光输出预算）只留在日志里。
    let mut last_batch_error: Option<String> = None;
    // P0-B：整批的「译文 == 原文」复制率统计
    let mut copy = CopyRate::default();
    // 输出校验自愈统计（回退 / 反思次数）：收尾返回给上层，并经 `TranslateEngine` 暴露。
    let mut heal = RewriteStats::default();

    // P0-C：单批源文本预算必须扣掉**每批重复注入**的固定开销——system 提示词模板
    // （含源语言线索与术语表）。术语表理论上限约 9.7k 字符，不扣就会顶爆小上下文
    // 服务端（400）或让输出被截断。
    //
    // 固定开销**本身就超窗**时（术语表一大就必然如此），预算会顶到下限，组出来的批
    // 是「超长提示词 + 极小 max_tokens」——服务端必然回 400，而 400 又会被判 Fatal
    // 让整片第一批就中止。这里先裁术语表再发，而不是硬着头皮发一个注定被拒的请求。
    // 本地克隆一份配置：裁过的术语表要真的用于本轮的提示词注入。
    let mut cfg = cfg.clone();
    let budget = online_char_budget(online_fixed_chars(&cfg.glossary_hint, ""));
    if budget.oversubscribed {
        let (fitted, trimmed) = fit_glossary_to_window(&cfg.glossary_hint, "");
        if trimmed {
            warn!(
                original_chars = cfg.glossary_hint.chars().count(),
                fitted_chars = fitted.chars().count(),
                "术语表过长，已按顺序裁剪到预算内（固定开销本身超窗，不裁则每次请求都会被服务端拒为 400）"
            );
            cfg.glossary_hint = fitted;
        }
    }
    let char_budget = online_char_budget(online_fixed_chars(&cfg.glossary_hint, "")).char_budget;

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
            &cfg,
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
                // 配置类错误（401/403/404：密钥、地址、模型名）不是暂时性的，
                // 后面的批会以完全相同的原因再失败一遍。继续「逐批跳过」只会把
                // 一个配置错误伪装成「翻译完成 0 句」。立刻带着真实原因中止。
                //
                // 其余一律按「本批软失败」处理并继续后面的句子：重试用尽（429/网络）
                // 与确定性的单批失败（推理模型的思考吃光输出预算）都只坏这一批，
                // 没有理由把前面已经译好的句子一起丢掉。
                match classify_batch_error(&err) {
                    BatchErrorAction::Fatal => {
                        // 中止前先把已译好的部分存进回传槽：界面失败分支会合并它，
                        // 不让本次已算好、已计费的译文随中止一起作废。
                        stash_partial(partial, &segments);
                        return Err(err).map_err(|e| {
                            anyhow!("在线翻译中止：{e}。请检查「性能设置 → 在线翻译 API」的地址、密钥与模型名。")
                        });
                    }
                    // 因长度被拒（400/413 且错误体提示上下文/长度）：折半后重发。
                    // 术语表 + 长行的组合顶爆上下文时，把一批拆小是最直接的出路——
                    // 整片中止或整片跳过都会白丢这一批本可译出的句子。
                    BatchErrorAction::HalveBatch if chunk.len() > 1 => {
                        let half = chunk.len().div_ceil(2);
                        warn!(
                            batch = chunk.len(),
                            "在线翻译本批因长度被拒（400/413），折半后重发"
                        );
                        let parts: Vec<Vec<usize>> =
                            chunk.chunks(half).map(|c| c.to_vec()).collect();
                        // 保序插回队首：先插后半批，前半批最终排在队首。
                        for part in parts.into_iter().rev() {
                            queue.push_front(part);
                        }
                        continue;
                    }
                    _ => {}
                }
                last_batch_error = Some(err.to_string());
                soft_failed_batches += 1;
                warn!(
                    error = %err,
                    "在线翻译本批失败（已重试 {} 次），跳过该批继续后续句子",
                    MAX_BATCH_RETRIES
                );
                // 本批**一句都没写回**：完成数不得增加。此前这里把整批计成已完成，
                // 于是进度条冲到 100% 之后，用户重试或收尾补译时又会回落——
                // 看起来像「翻译完了又重新开始」。进度只反映真实产出的句子。
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
        // 「改动过大」的可疑译文（长度失控 / 语义漂移）：先反思重试一次，仍不合格才留空。
        let mut suspect_candidates: Vec<usize> = Vec::new();
        copy.checked += chunk.len();
        // 校验口径与润色链路一致：`checked` = 本批实际产出、送交校验的条数。
        heal.checked += translations.len();
        for &pos in &chunk {
            if let Some(text) = translations.get(&segments[pos].index) {
                // 逐句输出校验：Ok 写回；Echo（复制原文，含近似复制）走已有的折半重试
                // 路径补译；SuspectTooDifferent（扩写/丢内容）留给反思重试；Empty 丢弃。
                // 绝不允许把可疑译文静默写进用户字幕。
                match validate_translation(&segments[pos].translate_source(), text) {
                    RewriteVerdict::Ok => {
                        segments[pos].translation = Some(text.clone());
                        segments[pos].translation_lang = Some(target_lang.to_string());
                        matched += 1;
                    }
                    RewriteVerdict::Echo => {
                        heal.echo += 1;
                        copy_candidates.push(pos);
                    }
                    RewriteVerdict::SuspectTooDifferent => suspect_candidates.push(pos),
                    RewriteVerdict::Empty => heal.empty += 1,
                }
            }
        }
        // 反思重试（**最多一次**）：只把可疑句重问一遍，要求最小必要改动；拿回结果后
        // 再过一遍校验，仍不合格就保持未译（与既有「未翻译」口径一致）。
        if !suspect_candidates.is_empty() {
            // 取消复核：主请求已经返回，但用户可能在这期间点了取消。反思请求同样是
            // **付费**请求，取消后不得再发（对照：折半路径与退避等待都有查取消）。
            if cancel.load(Ordering::Relaxed) {
                info!("在线翻译被取消，跳过对可疑句的反思重试");
                break;
            }
            let triples: Vec<(usize, String, String)> = suspect_candidates
                .iter()
                .map(|&pos| {
                    (
                        segments[pos].index,
                        segments[pos].translate_source(),
                        translations
                            .get(&segments[pos].index)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect();
            heal.reflect_calls += 1;
            match request_reflect_translation(&cfg, &triples, target_lang, &profile, &cancel) {
                Ok(healed) => {
                    for &pos in &suspect_candidates {
                        let source = segments[pos].translate_source();
                        match healed.get(&segments[pos].index) {
                            Some(text)
                                if validate_translation(&source, text) == RewriteVerdict::Ok =>
                            {
                                segments[pos].translation = Some(text.clone());
                                segments[pos].translation_lang = Some(target_lang.to_string());
                                matched += 1;
                            }
                            _ => heal.fallback += 1,
                        }
                    }
                }
                Err(err) => {
                    // 反思请求失败：不改动任何译文，可疑句全部按「未译」处理（回退）。
                    heal.fallback += triples.len();
                    warn!(error = %err, "在线翻译反思重试失败，{} 句按未译处理", triples.len());
                }
            }
            if heal.fallback > 0 {
                warn!(
                    suspects = triples.len(),
                    fallback = heal.fallback,
                    "在线翻译输出校验自愈：{} 句改动过大，反思后仍不合格，保持未译",
                    heal.fallback
                );
            }
        }
        if matched < chunk.len() {
            // 本批没译全：把漏掉的句子**对半重试**，而不是整批丢弃。
            // 输出被截断时，拆成更小的批通常一轮就能补齐；实在补不上才计入软失败。
            let missing: Vec<usize> = chunk
                .iter()
                .copied()
                .filter(|&pos| !segments[pos].translation_matches(target_lang))
                // 可疑句已走过「反思重试一次」的额度，不再进入折半重试（否则等于无限重试）。
                .filter(|pos| !suspect_candidates.contains(pos))
                .collect();
            let recovered = retry_missing_online(
                &cfg,
                &mut segments,
                &missing,
                target_lang,
                &cancel,
                &profile,
                &mut last_batch_error,
            );
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
        // 进度只统计**真的写回译文的句子数**。此前无条件 `+= chunk.len()`：本批若
        // 有句子没译出（输出被截断、被输出校验拒收、折半补译也没补回），它们同样
        // 被算作「已完成」，进度条冲到 100% 再回落，或让用户以为已经全部译完。
        // 只数「现在确实带目标语言译文」的句子——`translatable` 已排除开工前就有
        // 译文的句子，所以这个差值就是本批真正写回的条数。
        completed += chunk
            .iter()
            .filter(|&&pos| segments[pos].translation_matches(target_lang))
            .count();
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
            stash_partial(partial, &segments);
            return Err(anyhow!(
                "在线翻译质量异常：{}/{} 句译文与原文完全相同（复制率 {:.0}%），已按「未译出」处理。请检查模型是否适合该目标语言（可换更大的模型）。",
                copy.rejected,
                copy.checked,
                copy.rate() * 100.0
            ));
        }
        if ok == 0 {
            let reason = last_batch_error
                .as_deref()
                .map(|e| format!("最后一批的失败原因：{e}。"))
                .unwrap_or_default();
            stash_partial(partial, &segments);
            return Err(anyhow!(
                "在线翻译未能译出任何一句（共 {pending_total} 句待译）。{reason}请用「性能设置 → 在线翻译 API → 测试连接」检查地址、密钥与模型名。"
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
    // 发生过自愈就通过进度回调提示上层——「不允许把可疑内容静默写进用户字幕」。
    if heal.has_selfheal() {
        if let Some(ref cb) = progress_cb {
            cb(1.0, &heal.warning_message());
        }
    }
    Ok((segments, heal))
}

/// 把一批里**没译出**的句子按更小的批重试，返回补回的条数。
///
/// 与离线链路同一套思路：输出被截断时整批丢弃会让用户看到「翻译少了几句」，
/// 拆成更小的批（折半）通常一轮就能补齐。最多折半到单句；对某句反复补不上
/// （模型就是吐不出该序号）则放弃，交由上层计入软失败并提示用户可再点一次补译。
///
/// `last_error` 是收尾错误文案用的「最后一批失败原因」槽：折半补译里的请求失败
/// 同样要写进去，否则整片一句都没译出时用户只看到「原因未知」。
fn retry_missing_online(
    cfg: &OnlineApiConfig,
    segments: &mut [Segment],
    missing: &[usize],
    target_lang: &str,
    cancel: &AtomicBool,
    profile: &ModelProfile,
    last_error: &mut Option<String>,
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
                            // 与主路径**用同一条校验**（`validate_translation`）。主路径有
                            // 三档拒收（一字不差 / 相似度 ≥0.97 的近似复制 / 长度失控），
                            // 折半路径此前只判「完全等于原文」，于是「改一个功能字 / 加个
                            // 标点 / 扩写」的近似复制会**静默写进用户字幕**，还绕过了复制率
                            // 兜底。这里复用同一判据：非 Ok 一律保持未译。
                            if validate_translation(&segments[pos].translate_source(), text)
                                == RewriteVerdict::Ok
                            {
                                segments[pos].translation = Some(text.clone());
                                segments[pos].translation_lang = Some(target_lang.to_string());
                                recovered += 1;
                            } else {
                                debug!(
                                    index = segments[pos].index,
                                    "折半补译结果未通过输出校验，保持未译"
                                );
                                still.push(pos);
                            }
                        } else {
                            still.push(pos);
                        }
                    }
                }
                // 请求整体失败：不再继续折半（多半是网络/限流，继续只会更糟），
                // 把剩余的留给上层计入软失败。失败原因**必须**与主路径同款记录：
                // 整片一句都没译出时，收尾错误文案里的「最后一批失败原因」正是
                // 取自这里，若静默吞掉，用户看到的就是「原因未知」。
                Err(err) => {
                    warn!(
                        error = %err,
                        remaining = sub.len(),
                        "在线翻译折半补译请求失败，本批保持未译"
                    );
                    *last_error = Some(err.to_string());
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

/// 可取消的退避等待，返回 `false` 表示等待期间被取消。
///
/// 单次退避就有 0.6~1.8 秒，直接 `sleep` 会让「取消」最多迟滞这么久才生效，
/// 而且睡醒后还会再发一次请求。这里把整段等待切成小片，片间检查取消标志：
/// 一旦用户点了取消就立刻返回 `false`，调用方随即以「翻译已取消」退出，
/// 上层据此走**取消语义**（保留已完成的部分），而不是把它当成该批失败。
fn wait_with_cancel(total: Duration, cancel: &AtomicBool) -> bool {
    // 睡眠切片远小于最短退避（600ms）：取消几乎即时生效。
    let slice = Duration::from_millis(50);
    let mut left = total;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        if left.is_zero() {
            return true;
        }
        let step = left.min(slice);
        std::thread::sleep(step);
        left = left.saturating_sub(step);
    }
}

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
                // 等待本身必须可中断：用户点「取消」后不该再把这 0.6~1.8s 睡完，
                // 更不该睡醒后再发一次请求（[`wait_with_cancel`]）。
                if !wait_with_cancel(Duration::from_millis(600 * (attempt as u64 + 1)), cancel) {
                    return Err(anyhow!("翻译已取消"));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("在线翻译请求失败")))
}

/// 单批失败的**处置类别**：决定上层的下一步动作。
///
/// 此前只有「可重试 / 不可重试」二分，而「不可重试」在上层
/// （[`translate_via_online_api`]）被当成**中止整片**——这对三类错误过于粗暴：
/// 配置类错误（401/403/404）确实该立刻中止，但「推理模型的思考吃光输出预算」
/// 只坏在这一批，没有理由把前面已经译好的几百句一起丢掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchErrorAction {
    /// 暂时性（限流 / 5xx / 网络抖动 / 空响应 / 响应体读不出）：原样重发就有机会成功。
    Retry,
    /// 只坏这一批、且重发必然同样（推理模型的输出预算被思考吃光）：
    /// 跳过本批继续后面的句子，保住已完成的部分。
    SkipBatch,
    /// 请求因**过长**被服务端拒绝（400/413 且错误体提示上下文/长度）：折半后重发。
    /// 术语表一大、单批就必然超窗；整片中止或静默跳过都会白丢已译好的部分。
    HalveBatch,
    /// 配置/参数类（401/403/404、与长度无关的 400、取消）：后面的批会以同样的原因失败，
    /// 立刻带着真实原因中止，免得把配置错误伪装成「翻译完成 0 句」。
    Fatal,
}

/// 单批失败该怎么处置：重试、折半、跳过本批，还是中止整片。
fn classify_batch_error(err: &anyhow::Error) -> BatchErrorAction {
    let msg = err.to_string();
    // 「拿到了响应但读不出 body / JSON」「body 为空」「连接被中断」这几类是我们自己发的
    // 文案，**没有明确的 4xx 业务语义**，全是暂时性的传输问题。必须**先**判：解析失败
    // 文案里带响应体片段，片段里若恰好出现「HTTP 400」之类字样，会被下面的状态码分支
    // 误判成 Fatal，把一次响应异常升级成整片中止。
    if is_response_body_failure(&msg) {
        return BatchErrorAction::Retry;
    }
    // 再按真实状态码判断。不能直接 `contains("HTTP 5")`：因为错误文案里还拼了服务端的
    // 响应体（snippet），一个 4xx 的响应体若恰好提到了「HTTP 5xx」，就会被误判为可重试而白等退避。
    if let Some(code) = http_status_code(&msg) {
        return match code {
            429 | 500..=599 => BatchErrorAction::Retry,
            // 400/413 且错误体在抱怨长度：折半重发，绝不整片中止。
            400 | 413 if is_length_rejection(&msg) => BatchErrorAction::HalveBatch,
            _ => BatchErrorAction::Fatal,
        };
    }
    // 无状态码：ureq 的网络类错误，文案里带「无法连接」或底层 io 超时
    if msg.contains("无法连接") || msg.contains("timed out") || msg.contains("timeout") {
        return BatchErrorAction::Retry;
    }
    // 空响应（网关抖动、上游 200 空体、偶发只回思考）是**暂时性**的：重发一次往往就好。
    // 必须与下面「思考吃光预算」分开判——那条是确定性的，同一条 payload 重发只会再空一次。
    if msg.contains("在线接口返回了空内容") {
        return BatchErrorAction::Retry;
    }
    if msg.contains("思考过程耗尽输出预算") {
        return BatchErrorAction::SkipBatch;
    }
    BatchErrorAction::Fatal
}

/// 错误是否是「响应体读不出 / 为空 / 连接被中断」这类没有 4xx 业务语义的传输问题。
fn is_response_body_failure(msg: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "读取响应体失败",
        "响应体超过上限",
        "返回了空响应体",
        "响应不是合法 UTF-8",
        "不是合法 JSON",
    ];
    MARKERS.iter().any(|m| msg.contains(m))
}

/// 4xx 的响应体是否在抱怨「请求 / 上下文过长」。
///
/// 只认常见服务端措辞（英文小写比对 + 中文关键片），不做任何仅凭数字的猜测。
fn is_length_rejection(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const MARKERS: [&str; 10] = [
        "too long",
        "context length",
        "context_length",
        "maximum context",
        "too many tokens",
        "reduce the length",
        "prompt is too",
        "超过了最大",
        "上下文长度",
        "过长",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

/// 错误是否值得**原样重试**（[`BatchErrorAction::Retry`]）。
fn is_retryable(err: &anyhow::Error) -> bool {
    classify_batch_error(err) == BatchErrorAction::Retry
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

/// 在线 system 提示词模板（`request_batch_translation` 实际下发的那一份）。
///
/// **必须与 `request_batch_translation` 里的 `format!` 逐字一致**：预算推导
/// （[`ONLINE_SYSTEM_PROMPT_CHARS`]）直接取它的字符数，而 `max_output_tokens`
/// 也按它折算 prompt 长度。此前这里是手写的「约 130 汉字」估算，实测模板
/// 含全部格式约束后约 260 字符，低估近一倍会让预算虚高、极端术语表下更早
/// 撞上 400/截断。常量与模板漂移由 `online_prompt_template_chars_matches_estimate`
/// 测试守住。
///
/// 占位符：`{src_hint}` 源语言线索（可空）、`{glossary}` 术语表（可空）、
/// `{target_lang}` 目标语言名。
const ONLINE_SYSTEM_PROMPT_TEMPLATE: &str = "你是专业影视字幕翻译专家。{src_hint}{glossary}将用户给出的带序号字幕逐条翻译为地道的{target_lang}。\
【输出格式规范】\
必须且只能逐行输出，每行格式严格为「[序号] 译文」（严禁缺少括号与序号）。\
【硬性约束】\
1. 严禁添加任何问候、解释说明、Markdown代码块标记（如 ```）或前后赘述。\
2. 保持原意与语气，语言通顺紧凑，译文长度尽量与原文相当。\
3. 序号必须与原文完全一致，严禁遗漏任何一行，严禁合并多行，严禁改动序号。\
4. 数字、公式、变量名、编号与专有名词原样保留。";

/// 在线 system 提示词模板的字符数：**由模板自身推导**，不再手写估算。
///
/// 已含 `{src_hint}` / `{glossary}` / `{target_lang}` 三个占位符的字面量，
/// 调用方只需再叠加术语表与源语言线索的**实际**长度（见 [`online_fixed_chars`]）。
/// 编译期可用的「UTF-8 字符数」：`str::chars().count()` 不是 `const fn`，
/// 而 [`ONLINE_SYSTEM_PROMPT_CHARS`] 必须是常量（`char_budget` 在 const 上下文推导）。
///
/// 实现是标准的前导字节判定：`0b0xxx_xxxx` 与 `0b10xx_xxxx` 是续字节，不计。
const fn utf8_char_count(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut n = 0usize;
    while i < bytes.len() {
        // 续字节高位两位为 10；非续字节即一个新字符的起始。
        if bytes[i] & 0xC0 != 0x80 {
            n += 1;
        }
        i += 1;
    }
    n
}

const ONLINE_SYSTEM_PROMPT_CHARS: usize = utf8_char_count(ONLINE_SYSTEM_PROMPT_TEMPLATE);

/// 单批请求里**每批重复注入**的固定开销字符数：system 提示词模板
/// + 术语表 + 源语言线索，全部按实际长度计算。
///
/// 术语表（`config.rs` 允许 80 条 × 左右各 60 字符 ≈ 9.7k 字符）若不计入，
/// 一批字幕加提示词就会顶爆小上下文服务端（返回 400，而 4xx 被 `is_retryable`
/// 判为不可重试 → 整片翻译第一批就中止）。这里让预算显式感知它。
fn online_fixed_chars(glossary: &str, src_hint: &str) -> usize {
    // 模板自身已含 `{src_hint}` / `{glossary}` / `{target_lang}` 三个占位符
    // 字面量（合计 33 字符）。这里**不退还未使用的占位符字面量**，也不计真实
    // 目标语言名，只叠加术语表与源语言线索的实际长度。于是该值相对真实提示词
    // 长度是**保守高估**（偏差 = 33 − 目标语言名长度，`TRANSLATE_TARGET_LANGS`
    // 最长名 8 字符，故偏差 ≥ 25），预算只会偏紧不会偏松。
    ONLINE_SYSTEM_PROMPT_CHARS + glossary.chars().count() + src_hint.chars().count()
}

/// 单批**最低**可用源文本预算。
///
/// 不能再低：低于这个数连一条正常字幕（几十字）都装不下，组出来的批必然是
/// 「几乎全是提示词 + 极小正文」，服务端只会回 400。固定开销大到连它都塞不下时
/// 应当**先裁术语表**（见 [`fit_glossary_to_window`]），而不是把预算压到 0。
const MIN_ONLINE_CHAR_BUDGET: usize = 1_024;

/// 在线链路单批的预算判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OnlineBudget {
    /// 单批可用源文本字符预算（含 `[序号] ` 前缀），始终 ≥ [`MIN_ONLINE_CHAR_BUDGET`]。
    char_budget: usize,
    /// 固定开销（提示词模板 + 术语表 + 语言线索）是否已经**吃满** [`ONLINE_CHAR_CAP`]。
    /// 为真时无论怎么切批都必然超窗，调用方必须先裁术语表再发请求。
    oversubscribed: bool,
}

/// 在线链路单批的**可用源文本字符预算**：从 [`ONLINE_CHAR_CAP`] 里扣掉
/// [`online_fixed_chars`] 算出的固定开销，并报告固定开销是否已经超窗。
fn online_char_budget(fixed_chars: usize) -> OnlineBudget {
    OnlineBudget {
        char_budget: ONLINE_CHAR_CAP
            .saturating_sub(fixed_chars)
            .max(MIN_ONLINE_CHAR_BUDGET),
        oversubscribed: fixed_chars >= ONLINE_CHAR_CAP,
    }
}

/// 把术语表裁到**能放进预算**：保留前面的条目（用户优先写的），必要时截断最后一条。
///
/// 返回（裁剪后的术语表，是否发生裁剪）。已经放得下时原样返回。
///
/// 术语表是**每批重复注入**的固定开销，而 `config.rs` 允许上千条 × 60 字符，远超
/// [`ONLINE_CHAR_CAP`]。此时唯一的出路是**先裁它**——否则组出来的批必然是「超长提示词 +
/// 小 max_tokens」，服务端回 400。裁剪按「用户书写顺序」保留（前面的条目对用户更
/// 重要），最后一条按字符截断。
fn fit_glossary_to_window(glossary: &str, src_hint: &str) -> (String, bool) {
    // 设术语表占 g 字符：固定开销 = 模板 + g + 语言线索。要求裁完仍给正文留
    // MIN_ONLINE_CHAR_BUDGET，故 g ≤ CAP − MIN − 模板 − 线索。
    let room = ONLINE_CHAR_CAP.saturating_sub(
        ONLINE_SYSTEM_PROMPT_CHARS + src_hint.chars().count() + MIN_ONLINE_CHAR_BUDGET,
    );
    if glossary.chars().count() <= room {
        return (glossary.to_string(), false);
    }
    // `glossary_prompt()` 形如 `前缀：条目；条目。`；按 `；` 切条目，保住前缀。
    let (head, body) = match glossary.split_once('：') {
        Some((h, b)) => (format!("{h}："), b),
        None => (String::new(), glossary),
    };
    let body = body.strip_suffix('。').unwrap_or(body);
    let mut kept = String::new();
    let mut used = head.chars().count();
    if used <= room {
        kept.push_str(&head);
    } else {
        // 连前缀都放不下（极端配置）：截断前缀，至少别产出空提示。
        kept.extend(head.chars().take(room));
        used = room;
    }
    for (i, entry) in body.split('；').enumerate() {
        let sep = usize::from(i > 0);
        let entry_chars = entry.chars().count();
        if used + sep + entry_chars <= room {
            if sep == 1 {
                kept.push('；');
            }
            kept.push_str(entry);
            used += sep + entry_chars;
        } else {
            // 放不下整条：按字符截断这一条，别把整条丢掉。
            let take = room.saturating_sub(used + sep);
            if take > 0 {
                if sep == 1 {
                    kept.push('；');
                }
                kept.extend(entry.chars().take(take));
            }
            break;
        }
    }
    if room > 0 {
        kept.push('。');
    }
    (kept, true)
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
    let mut payload = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {
                "role": "system",
                // 必须与 [`ONLINE_SYSTEM_PROMPT_TEMPLATE`] **逐字一致**——预算推导与
                // `max_output_tokens` 都按该常量的字符数折算。`format!` 只接受字符串
                // 字面量，故此处不能直接引用常量；漂移由测试
                // `online_system_prompt_matches_template` 守住（它逐字比对本段）。
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
            online_fixed_chars(&cfg.glossary_hint, &src_hint),
            profile.is_reasoning()
        ),
        "stream": false,
    });
    if !supports_temperature(&cfg.model) {
        if let Some(obj) = payload.as_object_mut() {
            obj.remove("temperature");
        }
    }

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
    // 本批响应里到底有没有思考文本：首次响应为准，加大预算重试后按重试响应更新。
    let mut saw_thinking = observed > 0;
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
            saw_thinking = retry_observed > 0;
            content = extract_message_content(&value).unwrap_or_default();
        }
    }
    if content.trim().is_empty() {
        // 已知对面是推理模型、它却仍然只回了思考：输出预算已被思考吃光。这类空响应
        // 是**确定性**的——同一条 payload 再发一次只会再空一次，所以显式抛「预算耗尽」
        // （[`BatchErrorAction::SkipBatch`]：跳过本批、保住已译部分），
        // 而不是「空内容」（那条会被当成暂时性错误白白重试 3 轮）。
        if saw_thinking && profile.is_reasoning() {
            return Err(anyhow!(
                "推理模型思考过程耗尽输出预算，本批未产出译文（thinking 约占 {REASONING_THINKING_TOKENS} token，可降低每批条数后重试）"
            ));
        }
        return Err(anyhow!(
            "在线接口返回了空内容。若使用的是推理模型（如 deepseek-reasoner），请改用非推理模型"
        ));
    }

    let expected: HashSet<usize> = batch.iter().map(|seg| seg.index).collect();
    let mut parsed = LLMEngine::parse_batch_response(&content, &expected);

    // 输出被截断（`finish_reason=length`）时，**生成流末尾的那一条几乎必然是被切
    // 一半的**（模型写到一半就撞上 max_tokens）。麻烦在于这种半截行**仍能被
    // `parse_batch_response` 当成一条完整译文**——于是 `matched == chunk.len()`，
    // 上层的“本批未译全”分支不会触发，用户看到的就是一句被截断的译文。
    //
    // 「末尾那条」按**译文在响应体里最后出现的位置**判定，而不是按批内顺序
    // （见 [`truncation_victim`]）：模型偶尔乱序输出，批内顺序的最后一条可能是
    // 完整的，按批序丢它反而会把半截译文留在字幕里。
    //
    // 丢掉的那条会被上层当成“漏译”，由折半重试（`retry_missing_online`）补回来；
    // 即使它本来完整，重译一次也不会出错，只是多一次请求。
    if finish_reason_is_length(&value) {
        let batch_order: Vec<usize> = batch.iter().map(|s| s.index).collect();
        if let Some(idx) = truncation_victim(&content, &batch_order, &parsed) {
            parsed.remove(&idx);
            warn!(
                index = idx,
                batch = batch.len(),
                "在线翻译输出被截断，丢弃流末尾那条并交由折半重试补齐"
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

/// 对一批「改动过大」的可疑译文做**一次**反思重试（在线链路）。
///
/// 与 [`request_batch_translation`] 的差别只在提示词：系统段换成外置常量
/// [`REFLECT_PROMPT`]（附加目标语言），用户段**只列可疑句**（原句 + 被拒的上一版），
/// 要求「最小必要修改、长度相当」。解析与截断处理与主链路完全一致（同一个
/// [`LLMEngine::parse_batch_response`] 与 [`truncation_victim`]）。
fn request_reflect_translation(
    cfg: &OnlineApiConfig,
    suspects: &[(usize, String, String)],
    target_lang: &str,
    profile: &ModelProfile,
    cancel: &AtomicBool,
) -> Result<HashMap<usize, String>> {
    // 取消复核（与主请求同款）：主请求返回后用户可能已点取消，反思请求也是付费请求，
    // 取消后**不得**再发。放在拼 payload 之前，确保一个字节都不发出去。
    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("在线翻译已取消，跳过反思重试"));
    }
    let mut user = String::new();
    for (index, original, previous) in suspects {
        user.push_str(&format!(
            "[{index}] 原句：{original}\n上一版（改动过大，已弃用）：{previous}\n"
        ));
    }
    let source_chars: usize = suspects.iter().map(|(_, o, _)| o.chars().count()).sum();
    let base = source_chars * OUTPUT_TOKENS_PER_CHAR;
    let max_tokens = if profile.is_reasoning() {
        base.saturating_add(REASONING_THINKING_TOKENS)
            .clamp(MIN_OUTPUT_TOKENS, MAX_REASONING_OUTPUT_TOKENS)
    } else {
        base.clamp(MIN_OUTPUT_TOKENS, MAX_OUTPUT_TOKENS)
    };
    let payload = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {
                "role": "system",
                "content": format!(
                    "{REFLECT_PROMPT}（目标语言：{target_lang}；只翻译为{target_lang}，保持与原文长度相当。）\
    必须逐行输出，格式严格为「[序号] 译文」，不得解释、不得合并、不得遗漏、不得改动序号。"
                )
            },
            { "role": "user", "content": user }
        ],
        "temperature": 0.2,
        "max_tokens": max_tokens,
        "stream": false,
    });
    let value = post_chat_completions(cfg, &payload)?;
    let content = extract_message_content(&value).unwrap_or_default();
    if content.trim().is_empty() {
        return Err(anyhow!("在线接口反思重试返回了空内容"));
    }
    let expected: HashSet<usize> = suspects.iter().map(|(index, _, _)| *index).collect();
    let mut parsed = LLMEngine::parse_batch_response(&content, &expected);
    if finish_reason_is_length(&value) {
        let order: Vec<usize> = suspects.iter().map(|(index, _, _)| *index).collect();
        if let Some(idx) = truncation_victim(&content, &order, &parsed) {
            parsed.remove(&idx);
            warn!(
                index = idx,
                "在线反思重试输出被截断，丢弃该条（按未译处理）"
            );
        }
    }
    Ok(parsed)
}

/// 输出被截断时，挑出最可能是「被切一半」的那条已解析译文的序号。
///
/// 判据是**译文文本在响应体里最后出现的位置**：截断发生在生成流的末尾，最后写下的
/// 那条才是被切的。不能按「批内顺序的最后一条」来挑——模型偶尔会乱序输出（先写第
/// 3 句、再写第 2 句），此时批内顺序的最后一条其实是**完整**的，丢掉它、却把被切
/// 一半的那条当成完整译文写进字幕，正是这里要避免的静默截断。
///
/// 文本在响应体里定位不到时（个别服务端的应答片段被清洗过）回退到**批内倒序的
/// 第一条**，与旧行为一致。`batch_order` 是批内序号（升序）。
fn truncation_victim(
    content: &str,
    batch_order: &[usize],
    parsed: &HashMap<usize, String>,
) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (&index, text) in parsed {
        let needle = text.trim();
        if needle.is_empty() {
            continue;
        }
        let Some(offset) = content.rfind(needle) else {
            continue;
        };
        // 位置更靠后者胜出；位置相同（两条译文文本完全一致）时取序号更大者，
        // 这样结果与 HashMap 的遍历顺序无关。
        let better = match best {
            Some((best_offset, best_index)) => {
                offset > best_offset || (offset == best_offset && index > best_index)
            }
            None => true,
        };
        if better {
            best = Some((offset, index));
        }
    }
    if let Some((_, index)) = best {
        return Some(index);
    }
    batch_order
        .iter()
        .rev()
        .copied()
        .find(|index| parsed.contains_key(index))
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

/// 成功响应体的读取**字节上限**。
///
/// 错误分支此前就有上限（只取前 300 字符），成功分支却用 `into_json()` 把整个响应读进
/// 内存。没有 `Content-Length` 的响应（分块传输 / 网关流 / 失控的上游）能一直吐字节，
/// 足够把进程内存撑爆。正常 `chat/completions` 响应远小于此值。
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// 读响应体，带字节上限。
///
/// `ureq::Response::into_json()` 会把**所有**读取失败折成同一句「Failed to read JSON」，
/// 于是「连接被中途掐断 / 分块传输断了」与「服务端真的回了非法 JSON」在错误串里无法区分。
/// 这里自己读字节：
/// - 读取本身失败 → 「读取响应体失败（连接被中断或超时）」，判**可重试**；
/// - 超过 [`MAX_RESPONSE_BYTES`] → 明确超限，同样按可重试的暂时性故障处理。
fn read_response_body(resp: ureq::Response) -> Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| anyhow!("在线接口读取响应体失败（连接被中断或超时）: {e}"))?;
    if buf.len() > MAX_RESPONSE_BYTES {
        return Err(anyhow!(
            "在线接口响应体超过上限（>{MAX_RESPONSE_BYTES} 字节），已放弃该响应"
        ));
    }
    String::from_utf8(buf)
        .map_err(|e| anyhow!("在线接口读取响应体失败（响应不是合法 UTF-8 文本）: {e}"))
}

/// 解析响应体 JSON。空体 / 非 JSON 都在错误里带上响应体片段，便于排查。
///
/// 这类失败**没有明确的 4xx 业务语义**：拿到的是 WAF 页、半截 body（连接被掐断）或
/// 空 body，都是暂时性的传输问题。分类为可重试（见 [`classify_batch_error`]），
/// 绝不能因为一次响应异常就把整片已译好的任务作废。
fn parse_response_json(body: &str) -> Result<serde_json::Value> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "在线接口返回了空响应体（HTTP 200 但 body 为空，可能是网关/WAF 拦截或连接被截断）"
        ));
    }
    serde_json::from_str::<serde_json::Value>(trimmed).map_err(|e| {
        let snippet: String = trimmed.chars().take(160).collect();
        anyhow!(
            "在线接口响应不是合法 JSON: {e}；响应体前 {} 字符：{snippet}",
            snippet.chars().count()
        )
    })
}

/// 错误响应体透出给用户的最大字符数（错误文案要可读，不能把整张错误页贴进界面）。
const ERROR_BODY_SNIPPET_CHARS: usize = 300;

/// 把错误响应体的读取结果转成**展示片段**：`(片段, 截断提示)`。
///
/// - 读取成功 → 取前 [`ERROR_BODY_SNIPPET_CHARS`] 个字符，超长时附上截断提示；
/// - 读取失败 → 片段是**失败原因本身**（连接被中断 / 超过 4MB 上限 / 非 UTF-8），
///   绝不退化成空串。空串会让 401/404 的错误文案变成「服务端什么都没说」，
///   而真正的原因（响应体没读出来）反而消失。
fn error_body_snippet(body: Result<String>) -> (String, &'static str) {
    match body {
        Ok(body) => {
            let snippet: String = body.chars().take(ERROR_BODY_SNIPPET_CHARS).collect();
            // 只在真的被截断时才提示，免得短错误体上也挂一句无意义的「已截断」。
            let note = if snippet.chars().count() < body.chars().count() {
                "（响应体过长已截断）"
            } else {
                ""
            };
            (snippet, note)
        }
        Err(e) => (format!("读取错误响应体失败: {e}"), ""),
    }
}

/// 统一 POST 到 `/chat/completions` 并解析 JSON 响应体
///
/// 除翻译链路外，无头模式的「工具调用」入口也复用它：超时、鉴权、错误文案
/// 因此只有一份实现，外部调用方看到的失败原因与界面里完全一致。
pub(crate) fn post_chat_completions(
    cfg: &OnlineApiConfig,
    payload: &serde_json::Value,
) -> Result<serde_json::Value> {
    // 密钥里的换行/全角字符会让底层客户端以「非法头」拒绝整条请求，而那种错误
    // 既无状态码又会文案成「无法连接」，被判为可重试的暂时性故障。发请求前拦下。
    if let Some(message) = api_key_header_error(&cfg.api_key) {
        return Err(anyhow!(message));
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(cfg.timeout_secs.clamp(10, 600)))
        .build();

    let mut req = agent
        .post(&cfg.endpoint)
        .set("Content-Type", "application/json");

    let raw_key = cfg.api_key.trim();
    if !raw_key.is_empty() {
        let clean_key = raw_key
            .strip_prefix("Bearer ")
            .or_else(|| raw_key.strip_prefix("bearer "))
            .unwrap_or(raw_key)
            .trim();
        req = req.set("Authorization", &format!("Bearer {clean_key}"));
    }

    let response = req.send_json(payload.clone());

    match response {
        // 拿到了响应：再读体、再解析。任何「读不出 JSON」都单独成类（可重试），
        // 而不与 401/403/404 这类明确的业务错误混在一起。
        Ok(resp) => read_response_body(resp).and_then(|body| parse_response_json(&body)),
        Err(ureq::Error::Status(code, resp)) => {
            // 4xx/5xx 的响应体通常带服务端的错误说明，原样透出便于排查。
            // 同样带字节上限：错误页也可能是超大响应。
            //
            // 401/404 这种「最需要看服务端原文」的场景，恰恰也是「读不出体」最容易
            // 发生的场景（网关直接掐断连接）。此前 `unwrap_or_default()` 把读取失败
            // 抹成空串，用户只看到「HTTP 401: 」，服务端的线索全丢。这里把失败原因
            // 本身当片段透出。
            let (snippet, truncated) = error_body_snippet(read_response_body(resp));
            let hint = match code {
                401 | 403 => "（API Key 无效或无权限）",
                404 => "（接口地址或模型名不存在）",
                429 => "（触发限流，请降低批量条数或稍后重试）",
                _ => "",
            };
            Err(anyhow!(
                "在线接口返回 HTTP {code}{hint}: {snippet}{truncated}"
            ))
        }
        Err(e) => Err(anyhow!("无法连接在线翻译接口 {}: {e}", cfg.endpoint)),
    }
}

/// 校验 API Key 能否安全放进 `Authorization` 请求头；不合法时返回可操作的中文原因。
///
/// HTTP 头字段值只允许 `0x20..=0x7E` 与 TAB。从网页或聊天窗口复制密钥时常带上
/// 换行、全角字符或弯引号，底层 HTTP 客户端会在组装请求时以「非法头」直接拒绝整条
/// 请求；而这类错误既没有状态码、文案又是「无法连接」，会被 [`classify_batch_error`]
/// 当成**暂时性**故障重试 3 次，最终只报「未能译出任何一句」——真正的根因（密钥里
/// 混进了非法字符）被彻底藏起来。这里在发请求前拦下，直接指出是哪个字符不合法。
///
/// 首尾空白由 `trim` 容忍（复制密钥时最常见的尾随换行/空格不影响使用）。
fn api_key_header_error(raw_key: &str) -> Option<String> {
    let key = raw_key.trim();
    let bad = key.chars().find(|c| !matches!(*c, '\t' | ' '..='~'))?;
    let column = key.chars().take_while(|c| *c != bad).count() + 1;
    Some(format!(
        "在线翻译 API Key 含有无法放进 HTTP 请求头的字符 `{bad}`（U+{:04X}，第 {column} 个字符）：\
         多半是从网页复制时带上了换行或全角字符。请重新粘贴只含 ASCII 可见字符的密钥。",
        bad as u32
    ))
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
pub(crate) fn extract_message_content(value: &serde_json::Value) -> Option<String> {
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
        // 「拿到了响应但读不出 JSON」没有 4xx 业务语义（响应体可能是 WAF 页 / 半截
        // body）→ 可重试。回归：此前它落进 Fatal，一次响应异常就作废了整片已译内容。
        assert!(is_retryable(&anyhow!(
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
        // 空响应（网关抖动 / 上游 200 空体 / 偶发只回思考）同样是暂时性的：重发往往就好。
        // 回归：此前它不属于任何分支 → 判「不可重试」→ 上层直接中止整片。
        assert!(is_retryable(&anyhow!(
            "在线接口返回了空内容。若使用的是推理模型（如 deepseek-reasoner），请改用非推理模型"
        )));
    }

    /// 单批失败的处置类别：暂时性重试、确定性的单批失败跳过、配置类中止整片。
    ///
    /// 回归：此前只有「可重试 / 不可重试」二分，而「不可重试」在上层被当成**中止整片**
    /// ——推理模型把输出预算全花在思考上时，几百句已译好的字幕会被一起丢掉。
    #[test]
    fn batch_error_action_classification() {
        use BatchErrorAction::{Fatal, Retry, SkipBatch};
        assert_eq!(
            classify_batch_error(&anyhow!(
                "在线接口返回 HTTP 429（触发限流，请降低批量条数或稍后重试）"
            )),
            Retry
        );
        assert_eq!(
            classify_batch_error(&anyhow!(
                "在线接口返回了空内容。若使用的是推理模型（如 deepseek-reasoner），请改用非推理模型"
            )),
            Retry
        );
        // 确定性但只坏这一批：跳过本批继续后面的句子，绝不中止整片
        assert_eq!(
            classify_batch_error(&anyhow!(
                "推理模型思考过程耗尽输出预算，本批未产出译文（thinking 约占 2048 token，可降低每批条数后重试）"
            )),
            SkipBatch
        );
        // 配置类：后面的批会以同样的原因失败，立刻中止
        for msg in [
            "在线接口返回 HTTP 401（API Key 无效或无权限）: unauthorized",
            "在线接口返回 HTTP 403（API Key 无效或无权限）",
            "在线接口返回 HTTP 404（接口地址或模型名不存在）",
            "在线接口返回 HTTP 400: upstream said HTTP 500, bad request",
        ] {
            assert_eq!(classify_batch_error(&anyhow!("{msg}")), Fatal, "{msg}");
        }
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

    /// 输出校验自愈（在线链路）：译文「改动过大」（把一句扩写成一整段）时，先做
    /// **一次**反思重试；仍不合格则回退成「未译」，绝不写进字幕；正常句照常写回。
    #[test]
    fn online_runaway_translation_falls_back_after_one_reflection() {
        let runaway = "这是一段".to_string() + &"扩写内容".repeat(30);
        let normal = serde_json::json!({
            "choices": [{ "message": { "content": format!("[1] {runaway}\n[2] Hello world.") } }]
        })
        .to_string();
        let reflect = serde_json::json!({
            "choices": [{ "message": { "content": format!("[1] {runaway}") } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![normal, reflect]);
        let cfg = online_cfg_for(&addr);

        let segs = vec![
            Segment::new(1, 0.0, 1.0, "这句只有十几个字的一句话"),
            Segment::new(2, 1.0, 2.0, "你好世界"),
        ];
        let (out, stats) = translate_via_online_api_with_stats(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .expect("mock 服务器应返回成功");

        // 可疑句回退成「未译」
        assert_eq!(out[0].translation, None, "改动过大的译文不得写回");
        assert_eq!(out[0].translation_lang, None);
        // 正常句照常写回
        assert_eq!(out[1].translation.as_deref(), Some("Hello world."));
        // 反思只重试一次，回退统计正确
        assert_eq!(stats.reflect_calls, 1, "反思只允许重试一次");
        assert_eq!(stats.fallback, 1);
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "应为「首次 + 一次反思」共两次请求"
        );
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

    /// 日语等外语源文本翻译为「简体中文」时，严禁短路走 zhconv，必须真正请求在线 API。
    #[test]
    fn japanese_source_to_chinese_target_does_not_short_circuit_and_calls_api() {
        let resp = serde_json::json!({
            "choices": [{ "message": { "content": "[1] 你好，今天天气真好。" } }]
        })
        .to_string();
        let (addr, seen) = spawn_mock_server(vec![resp]);
        let cfg = online_cfg_for(&addr);

        let mut seg = Segment::new(1, 0.0, 1.0, "こんにちは、今日はいい天気ですね。");
        seg.language = Some("ja".to_string());
        let segs = vec![seg];

        let out = TranslateEngine::online(cfg)
            .translate_subtitles(segs, "简体中文", None, Arc::new(AtomicBool::new(false)))
            .expect("日译中应成功走在线 API");

        assert_eq!(seen.lock().unwrap().len(), 1, "日译中必须发送一次在线请求");
        assert_eq!(out[0].translation.as_deref(), Some("你好，今天天气真好。"));
        assert_eq!(out[0].translation_lang.as_deref(), Some("简体中文"));
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
        assert_eq!(base.char_budget, ONLINE_CHAR_CAP);
        assert!(!base.oversubscribed, "无固定开销时不该超窗");
        let with_glossary = online_char_budget(2_000);
        assert!(
            with_glossary.char_budget < base.char_budget,
            "术语表开销必须压缩在线预算: {} vs {}",
            with_glossary.char_budget,
            base.char_budget
        );
        // 术语表长到超过上限时也不能归零（留最小可用批），并且必须报告「超窗」
        let huge = online_char_budget(50_000);
        assert!(huge.char_budget >= MIN_ONLINE_CHAR_BUDGET);
        assert!(huge.oversubscribed, "固定开销吃满窗口必须报告超窗");
    }

    /// 术语表很长时，在线批次应收缩到「固定开销 + 本批 + 输出」仍能放进上下文。
    #[test]
    fn online_batches_shrink_when_glossary_is_huge() {
        let segs: Vec<Segment> = (1..=60)
            .map(|i| Segment::new(i, 0.0, 1.0, "字".repeat(200)))
            .collect();
        let pending: Vec<usize> = (0..60).collect();

        let no_glossary =
            plan_online_batches(&pending, &segs, 40, online_char_budget(0).char_budget);
        let huge = "术语表（以下词条必须按给定译法翻译，不得改写）：".to_string()
            + &"甲=乙；".repeat(1_000);
        let fixed = online_fixed_chars(&huge, "");
        let budget = online_char_budget(fixed).char_budget;
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

    /// 预算推导用的 system 提示词字符数必须等于**真实模板**的字符数。
    ///
    /// 修前这里是手写的「约 130 汉字」估算，而实际下发的模板（含全部格式
    /// 约束）约 260 字符——低估近一倍会让 `char_budget` 虚高、`max_output_tokens`
    /// 的可用输出被高估，极端术语表下更早撞上服务端 400 或输出截断。
    /// 这条测试把「常量」与「模板」绑死，改文案忘了改常量就会红。
    #[test]
    fn online_prompt_template_chars_matches_estimate() {
        assert_eq!(
            ONLINE_SYSTEM_PROMPT_CHARS,
            ONLINE_SYSTEM_PROMPT_TEMPLATE.chars().count(),
            "字符数常量必须由模板自身推导，禁止再手写估算"
        );
        // 模板必须真的含三个占位符，否则 format! 会静默漏注入。
        assert!(ONLINE_SYSTEM_PROMPT_TEMPLATE.contains("{src_hint}"));
        assert!(ONLINE_SYSTEM_PROMPT_TEMPLATE.contains("{glossary}"));
        assert!(ONLINE_SYSTEM_PROMPT_TEMPLATE.contains("{target_lang}"));
        // 模板本身就得有分量：小于 150 字符说明格式约束被删了。
        // 模板本身就得有分量：小于 150 字符说明格式约束被删了。
        // 直接断言常量会被 clippy::assertions_on_constants 拦下，故用「实际
        // 推导一次」的调用形式（同时覆盖 online_fixed_chars 的接线）。
        assert!(
            online_fixed_chars("", "") >= 150,
            "模板过短（{} 字符），格式约束疑似丢失",
            ONLINE_SYSTEM_PROMPT_CHARS
        );
    }

    /// 固定开销 = 模板 + 术语表 + 源语言线索，三项都必须按**实际**长度计入。
    #[test]
    fn online_fixed_chars_counts_every_injected_part() {
        assert_eq!(online_fixed_chars("", ""), ONLINE_SYSTEM_PROMPT_CHARS);
        assert_eq!(
            online_fixed_chars("甲=乙", ""),
            ONLINE_SYSTEM_PROMPT_CHARS + 3
        );
        assert_eq!(
            online_fixed_chars("", "源语言为中文；"),
            ONLINE_SYSTEM_PROMPT_CHARS + 7
        );
        // 两项同时注入时线性相加，不能只加其中一项。
        assert_eq!(
            online_fixed_chars("甲=乙", "源语言为中文；"),
            ONLINE_SYSTEM_PROMPT_CHARS + 3 + 7
        );
    }

    /// 实际下发的 system 提示词必须与 [`ONLINE_SYSTEM_PROMPT_TEMPLATE`] 逐字一致。
    ///
    /// `format!` 只接受字符串字面量，所以 `request_batch_translation` 里不能
    /// 直接引用常量。这里把「占位符填好后的真实字符串」与常量对齐，改文案
    /// 忘了改常量（或反过来）都会红。
    #[test]
    fn online_system_prompt_matches_template() {
        // `format!` 只接受字符串字面量，故用 replace 复刻同一套占位符替换。
        let rendered = ONLINE_SYSTEM_PROMPT_TEMPLATE
            .replace("{src_hint}", "源语言为中文；")
            .replace(
                "{glossary}",
                "术语表（以下词条必须按给定译法翻译，不得改写）：甲=乙；",
            )
            .replace("{target_lang}", "English");
        // 常量里的占位符必须全部被替换掉（不留 `{...}` 残迹）。
        assert!(!rendered.contains("{src_hint}"), "源语言线索占位符未替换");
        assert!(!rendered.contains("{glossary}"), "术语表占位符未替换");
        assert!(!rendered.contains("{target_lang}"), "目标语言占位符未替换");
        // 三段注入内容都必须出现在成品提示词里。
        assert!(rendered.contains("源语言为中文；"));
        assert!(rendered.contains("甲=乙"));
        assert!(rendered.contains("地道的English"));
        // 成品长度 = 模板 − 三个占位符字面量 + 三段实际注入内容（证明没有
        // 第四处暗改）。替换是「占位符 -> 内容」，占位符自身的字面长度必须
        // 显式扣掉，否则会重复计数 33 字符。
        let placeholder_chars = "{src_hint}{glossary}{target_lang}".chars().count();
        let glossary = "术语表（以下词条必须按给定译法翻译，不得改写）：甲=乙；";
        assert_eq!(
            rendered.chars().count(),
            ONLINE_SYSTEM_PROMPT_CHARS - placeholder_chars
                + "源语言为中文；".chars().count()
                + glossary.chars().count()
                + "English".chars().count(),
            "成品提示词长度与模板-占位符+注入项不符"
        );
        // 预算模型（[`online_fixed_chars`]）把占位符字面量也计入固定开销，
        // 于是对真实提示词长度是**保守高估**：估算值 + 目标语言名长度恒 ≥
        // 真实长度（差 = 33 − 目标语言名长度 ≥ 0，见 TRANSLATE_TARGET_LANGS
        // 的最长名 "Français" 仅 8 字符）。预算只会偏紧，不会因低估顶爆上下文。
        assert!(
            rendered.chars().count()
                <= online_fixed_chars(glossary, "源语言为中文；") + "English".chars().count(),
            "固定开销估算低于真实提示词长度，预算方向性错误"
        );
        // 格式约束一条都不能少：截断到硬性约束块，逐条断言关键词仍在。
        for kw in [
            "【输出格式规范】",
            "【硬性约束】",
            "严禁添加任何问候",
            "严禁遗漏任何一行",
            "严禁合并多行",
            "严禁改动序号",
            "原样保留",
        ] {
            assert!(rendered.contains(kw), "提示词丢失约束：{kw}");
        }
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
            msg.contains("耗尽输出预算"),
            "应报「思考吃光输出预算」而不是静默成功: {msg}"
        );
        assert!(
            msg.contains("未能译出任何一句"),
            "本批失败应记为软失败，并在最终错误里带上根因: {msg}"
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

    /// P0-C 端到端回归：网关**持续**返回空内容时，只该跳过这一批，
    /// 绝不能把前面已经译好的批次一起丢掉。
    ///
    /// 修前：空内容错误不在 `is_retryable` 的任一分支里 → 判「不可重试」→
    /// 上层直接 `return Err` 中止整片，用户拿到的是「一句都没译出」。
    #[test]
    fn empty_content_skips_batch_and_keeps_earlier_translations() {
        let ok = serde_json::json!({
            "choices": [{ "message": { "content": "[1] Hello." } }]
        })
        .to_string();
        let empty = serde_json::json!({
            "choices": [{ "message": { "content": "" } }]
        })
        .to_string();
        // 第 1 批成功；第 2 批连回 MAX_BATCH_RETRIES 次空内容（重发仍空 → 跳过该批）
        let mut replies = vec![ok];
        replies.extend(std::iter::repeat_n(empty, MAX_BATCH_RETRIES));
        let (addr, seen) = spawn_mock_server(replies);
        let mut cfg = online_cfg_for(&addr);
        cfg.batch_size = 1; // 每条一批，强制产生两批

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
        .expect("只有一批失败时不该整片失败");

        assert_eq!(
            out[0].translation.as_deref(),
            Some("Hello."),
            "已译好的批次必须保住"
        );
        assert!(out[1].translation.is_none(), "空内容批次不该凭空产生译文");
        assert_eq!(
            seen.lock().unwrap().len(),
            1 + MAX_BATCH_RETRIES,
            "空响应应重试到上限再跳过该批"
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

    /// 回归：拿到 200 却读不出 JSON（WAF 页 / 半截 body）、空响应体、连接被中断，
    /// 这几类都**没有** 4xx 业务语义 → 必须判可重试，绝不能升级成整片中止。
    #[test]
    fn response_body_failures_classify_as_retryable() {
        use BatchErrorAction::{Fatal, HalveBatch, Retry};
        for msg in [
            "在线接口返回了空响应体（HTTP 200 但 body 为空，可能是网关/WAF 拦截或连接被截断）",
            "在线接口响应不是合法 JSON: expected value at line 1；响应体前 5 字符：<html",
            "在线接口读取响应体失败（连接被中断或超时）: unexpected end of file",
            "在线接口响应体超过上限（>4194304 字节），已放弃该响应",
        ] {
            assert_eq!(classify_batch_error(&anyhow!("{msg}")), Retry, "{msg}");
        }
        // 解析失败文案里带响应体片段：片段里恰好出现「HTTP 400」时也不能被状态码
        // 分支误判成 Fatal，把一次响应异常升级成整片中止。
        assert_eq!(
            classify_batch_error(&anyhow!(
                "在线接口响应不是合法 JSON: expected value；响应体前 40 字符：<html>HTTP 400 Bad Request</html>"
            )),
            Retry
        );
        // 鉴权 / 地址 / 模型名错误仍然 Fatal，不能一股脑全改可重试。
        for msg in [
            "在线接口返回 HTTP 401（API Key 无效或无权限）: unauthorized",
            "在线接口返回 HTTP 403（API Key 无效或无权限）",
            "在线接口返回 HTTP 404（接口地址或模型名不存在）",
            "在线接口返回 HTTP 400: bad request",
        ] {
            assert_eq!(classify_batch_error(&anyhow!("{msg}")), Fatal, "{msg}");
        }
        // 400/413 + 长度措辞 → 折半重发，而不是整片中止。
        assert_eq!(
            classify_batch_error(&anyhow!(
                "在线接口返回 HTTP 400: This model's maximum context length is 8192 tokens, reduce the length of the messages"
            )),
            HalveBatch
        );
        assert_eq!(
            classify_batch_error(&anyhow!("在线接口返回 HTTP 413: 请求体过长")),
            HalveBatch
        );
    }

    /// 端到端：第二批拿到 200 + 非 JSON 响应体时**不得**整片中止；重试 3 次后只跳过
    /// 这一批，第一批已译好的句子必须保留（修前：一次坏响应 → Fatal → 整片作废）。
    #[test]
    fn online_non_json_body_does_not_abort_the_whole_run() {
        let ok = serde_json::json!({ "choices": [{ "message": { "content": "[1] Hello." } }] })
            .to_string();
        let garbage = "<html><body>502 Bad Gateway</body></html>".to_string();
        let (addr, seen) = spawn_mock_server_status(vec![
            (200, ok),
            (200, garbage.clone()),
            (200, garbage.clone()),
            (200, garbage),
        ]);
        let mut cfg = online_cfg_for(&addr);
        cfg.batch_size = 1;
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
        .expect("响应体读不出是暂时性故障，不得整片中终止");
        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
        assert!(out[1].translation.is_none(), "坏响应的那批应保持未译");
        assert_eq!(
            seen.lock().unwrap().len(),
            4,
            "第二批应按 MAX_BATCH_RETRIES 重试 3 次"
        );
    }

    /// 端到端：HTTP 200 + 空响应体同样是暂时性传输故障 → 可重试、不整片中终止。
    #[test]
    fn online_empty_body_does_not_abort_the_whole_run() {
        let ok = serde_json::json!({ "choices": [{ "message": { "content": "[1] Hello." } }] })
            .to_string();
        let (addr, _seen) = spawn_mock_server_status(vec![
            (200, ok),
            (200, String::new()),
            (200, String::new()),
            (200, String::new()),
        ]);
        let mut cfg = online_cfg_for(&addr);
        cfg.batch_size = 1;
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
        .expect("空响应体不得整片中终止");
        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
    }

    /// 固定开销（术语表）本身就超窗时，预算函数必须报告；裁剪后必须能组出合法批。
    #[test]
    fn oversubscribed_glossary_is_trimmed_to_fit_window() {
        let huge = "术语表（以下词条必须按给定译法翻译，不得改写）：".to_string()
            + &"甲甲甲甲=乙乙乙乙；".repeat(1_000);
        let budget = online_char_budget(online_fixed_chars(&huge, ""));
        assert!(budget.oversubscribed, "固定开销吃满窗口时必须报告超窗");
        assert!(budget.char_budget >= MIN_ONLINE_CHAR_BUDGET);

        let (fitted, trimmed) = fit_glossary_to_window(&huge, "");
        assert!(trimmed, "超窗时必须裁剪术语表");
        assert!(fitted.chars().count() < huge.chars().count());
        assert!(
            fitted.starts_with("术语表（以下词条必须按给定译法翻译，不得改写）："),
            "前缀必须保留: {fitted}"
        );
        let fitted_budget = online_char_budget(online_fixed_chars(&fitted, ""));
        assert!(!fitted_budget.oversubscribed, "裁剪后不得再超窗");
        assert!(fitted_budget.char_budget >= MIN_ONLINE_CHAR_BUDGET);

        let segs: Vec<Segment> = (1..=20)
            .map(|i| Segment::new(i, 0.0, 1.0, "字".repeat(120)))
            .collect();
        let pending: Vec<usize> = (0..20).collect();
        let batches = plan_online_batches(&pending, &segs, 40, fitted_budget.char_budget);
        assert!(!batches.is_empty());
        for b in &batches {
            let chars: usize = b
                .iter()
                .map(|&p| segs[p].translate_source().chars().count() + 6)
                .sum();
            assert!(
                chars <= fitted_budget.char_budget,
                "单批字符数应受预算约束: {chars} > {}",
                fitted_budget.char_budget
            );
        }

        // 放得下的术语表原样返回。
        let small = "术语表（以下词条必须按给定译法翻译，不得改写）：甲=乙。";
        let (kept, trimmed) = fit_glossary_to_window(small, "");
        assert!(!trimmed);
        assert_eq!(kept, small);
    }

    /// 端到端：超窗的术语表必须先被裁剪再注入请求，否则请求必然被服务端 400 拒绝。
    #[test]
    fn oversized_glossary_is_trimmed_before_request() {
        let reply = serde_json::json!({ "choices": [{ "message": { "content": "[1] Hello." } }] })
            .to_string();
        let (addr, seen) = spawn_mock_server_status(vec![(200, reply)]);
        let mut cfg = online_cfg_for(&addr);
        cfg.glossary_hint = "术语表（以下词条必须按给定译法翻译，不得改写）：".to_string()
            + &"甲甲乙乙=丙丙丁丁；".repeat(4_000);
        let segs = vec![Segment::new(1, 0.0, 1.0, "你好")];
        let out = translate_via_online_api(
            &cfg,
            segs,
            "English",
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("术语表超窗时应先裁剪再发请求");
        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
        let body = seen.lock().unwrap().join("\n");
        assert!(
            body.len() < 20_000,
            "术语表必须被裁剪后再注入: 请求体 {} 字节",
            body.len()
        );
    }

    /// 端到端：一批 2 句被服务端以「context length 超限」拒（400）时不得整片中止，
    /// 应折半重发，拆成单句后逐条成功。
    #[test]
    fn length_rejection_halves_batch_instead_of_aborting() {
        let too_long = serde_json::json!({
            "error": { "message": "This model's maximum context length is 8192 tokens" }
        })
        .to_string();
        let ok1 = serde_json::json!({ "choices": [{ "message": { "content": "[1] Hello." } }] })
            .to_string();
        let ok2 = serde_json::json!({ "choices": [{ "message": { "content": "[2] World." } }] })
            .to_string();
        let (addr, seen) = spawn_mock_server_status(vec![(400, too_long), (200, ok1), (200, ok2)]);
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
        .expect("长度类 400 不得整片中终止");
        assert_eq!(out[0].translation.as_deref(), Some("Hello."));
        assert_eq!(out[1].translation.as_deref(), Some("World."));
        assert_eq!(seen.lock().unwrap().len(), 3, "首次整批 + 折半两次");
    }

    /// 取消后反思请求**不得再发**（反思也是付费请求）；修前主请求返回与反思之间没有复查
    /// 取消标志，取消后仍会白发一次。
    #[test]
    fn reflect_request_is_skipped_when_already_cancelled() {
        let reply = serde_json::json!({ "choices": [{ "message": { "content": "[1] Hello." } }] })
            .to_string();
        let (addr, seen) = spawn_mock_server_status(vec![(200, reply)]);
        let cfg = online_cfg_for(&addr);
        let suspects = vec![(1usize, "你好世界".to_string(), "你好世界。".to_string())];
        let cancel = AtomicBool::new(true);
        let err = request_reflect_translation(
            &cfg,
            &suspects,
            "English",
            &ModelProfile::default(),
            &cancel,
        )
        .expect_err("已取消时不得发反思请求");
        assert!(err.to_string().contains("取消"), "{err}");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "取消后一个反思请求都不该发出"
        );
    }

    /// 折半补译回来的仍是「近似复制 / 扩写」时，必须与主路径同口径拒收：保持未译，
    /// 绝不静默写进用户字幕。修前折半路径只判「完全等于原文」，扩写会被静默采纳。
    #[test]
    fn halved_writeback_rejects_near_copy() {
        // 原文 11 字，译文 = 原文重复 9 次 → 长度比 9 > 8，判「改动过大」。
        let source = "今天天气真好我们出发吧";
        let runaway = source.repeat(9);
        let reply = serde_json::json!({
            "choices": [{ "message": { "content": format!("[1] {runaway}") } }]
        })
        .to_string();
        let (addr, _seen) = spawn_mock_server_status(vec![(200, reply)]);
        let cfg = online_cfg_for(&addr);
        let mut segs = vec![Segment::new(1, 0.0, 1.0, source)];
        let mut last_error = None;
        let recovered = retry_missing_online(
            &cfg,
            &mut segs,
            &[0],
            "English",
            &AtomicBool::new(false),
            &ModelProfile::default(),
            &mut last_error,
        );
        assert_eq!(recovered, 0, "近似复制不得计入已译");
        assert!(segs[0].translation.is_none(), "近似复制不得写回字幕");
    }

    /// 在线链路整片中终止（配置类 Fatal）时，已译好的部分必须留在部分结果槽里供界面
    /// 合并，而不是一次故障全部作废。
    #[test]
    fn fatal_abort_keeps_partial_translations_in_sink() {
        let ok = serde_json::json!({ "choices": [{ "message": { "content": "[1] Hello." } }] })
            .to_string();
        // 第一批成功、第二批收到 401（配置类 Fatal）→ 整片立即中止。
        let (addr, _seen) =
            spawn_mock_server_status(vec![(200, ok), (401, "unauthorized".to_string())]);
        let mut cfg = online_cfg_for(&addr);
        cfg.batch_size = 1;
        let segs = vec![
            Segment::new(1, 0.0, 1.0, "你好"),
            Segment::new(2, 1.0, 2.0, "世界"),
        ];
        let mut engine = TranslateEngine::online(cfg);
        let sink = PartialSink::default();
        engine.set_partial_sink(sink.clone());
        let err = engine
            .translate_subtitles(segs, "English", None, Arc::new(AtomicBool::new(false)))
            .expect_err("401 必须整片中终止");
        assert!(err.to_string().contains("在线翻译中止"), "{err}");

        let partial = sink
            .lock()
            .unwrap()
            .clone()
            .expect("Fatal 必须留下部分结果");
        assert_eq!(
            partial[0].translation.as_deref(),
            Some("Hello."),
            "第一批已译好的句子必须保留在部分结果里"
        );
        assert!(partial[1].translation.is_none());
    }

    /// `OnlineApiConfig` 的 `Debug` 必须给密钥打码：任何人后来加一行 `debug!(?cfg)`
    /// 都不该把明文密钥写进日志（日志常被贴进工单或聊天窗口）。
    #[test]
    fn online_api_config_debug_redacts_api_key() {
        let cfg = OnlineApiConfig {
            api_key: "sk-supersecretvalue123456".to_string(),
            ..sample_cfg()
        };
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("sk-supersecretvalue123456"),
            "完整密钥不得出现在 Debug 输出里: {rendered}"
        );
        assert!(
            !rendered.contains("supersecretvalue"),
            "密钥尾部不得泄露: {rendered}"
        );
        assert!(
            rendered.contains("api_key"),
            "仍要能看出哪个字段是密钥: {rendered}"
        );
        assert!(
            rendered.contains("sk-s"),
            "前 4 位应保留，便于确认填的是哪把 key: {rendered}"
        );
        // 其余字段照常可读（排障需要）
        assert!(rendered.contains("https://example.com/v1/chat/completions"));
        assert!(rendered.contains("test-model"));

        // 空 / 纯空白：显示 <empty>
        for blank in ["", "   "] {
            let empty = OnlineApiConfig {
                api_key: blank.to_string(),
                ..sample_cfg()
            };
            let text = format!("{empty:?}");
            assert!(text.contains("<empty>"), "空密钥应显示 <empty>: {text}");
        }

        // 短密钥连前 4 位也不暴露（前 4 位几乎等于泄露一半）
        let short = OnlineApiConfig {
            api_key: "abc123".to_string(),
            ..sample_cfg()
        };
        let text = format!("{short:?}");
        assert!(!text.contains("abc123"), "短密钥不得泄露: {text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    /// 错误响应体读取失败时必须保留**失败原因**，不能抹成空串：401/404 最需要看
    /// 服务端原文，而这时恰恰最容易读不出体。超长体则截断并明确提示。
    #[test]
    fn error_body_snippet_keeps_read_failure_reason() {
        let (text, note) = error_body_snippet(Err(anyhow!(
            "在线接口读取响应体失败（连接被中断或超时）: unexpected eof"
        )));
        assert!(text.contains("读取错误响应体失败"), "{text}");
        assert!(
            text.contains("unexpected eof"),
            "必须带上底层失败原因: {text}"
        );
        assert!(note.is_empty(), "读取失败时不需要截断提示: {note}");

        // 正常错误体：原文透出，短体不加截断提示
        let (short, note) = error_body_snippet(Ok("{\"error\":\"invalid api key\"}".to_string()));
        assert!(short.contains("invalid api key"), "{short}");
        assert!(note.is_empty(), "{note}");

        // 超长错误体：截断到上限并明确提示
        let long = "x".repeat(ERROR_BODY_SNIPPET_CHARS + 50);
        let (cut, note) = error_body_snippet(Ok(long));
        assert_eq!(cut.chars().count(), ERROR_BODY_SNIPPET_CHARS);
        assert!(note.contains("已截断"), "{note}");
    }

    /// 端到端：服务端回了一个超过 4MB 上限的错误体时 `read_response_body` 会失败，
    /// 但失败原因必须留在给用户的错误文案里（此前 `unwrap_or_default()` 把它抹平，
    /// 用户只看到「HTTP 401: 」）。
    #[test]
    fn oversized_error_body_keeps_read_failure_in_message() {
        let huge = "x".repeat(MAX_RESPONSE_BYTES + 16);
        let (addr, _seen) = spawn_mock_server_status(vec![(401, huge)]);
        let cfg = online_cfg_for(&addr);
        let err = post_chat_completions(&cfg, &serde_json::json!({ "model": "m" }))
            .expect_err("401 必须报错");
        let msg = err.to_string();
        assert!(msg.contains("HTTP 401"), "{msg}");
        assert!(
            msg.contains("读取错误响应体失败"),
            "必须保留读取失败原因而不是空串: {msg}"
        );
        assert!(msg.contains("超过上限"), "{msg}");
    }

    /// 折半补译的请求失败必须写进「最后一批失败原因」：整片一句都没译出时，收尾错误
    /// 文案正是从这里取原因，静默吞掉会让用户只看到「原因未知」。
    #[test]
    fn halving_retry_failure_records_reason_for_final_error() {
        // 400（非长度类）是不可重试错误 → `retry_missing_online` 立刻拿到 Err。
        let (addr, _seen) =
            spawn_mock_server_status(vec![(400, "{\"error\":\"bad request\"}".to_string())]);
        let cfg = online_cfg_for(&addr);
        let mut segs = vec![Segment::new(7, 0.0, 1.0, "hello world")];
        let mut last_error: Option<String> = None;
        let recovered = retry_missing_online(
            &cfg,
            &mut segs,
            &[0],
            "English",
            &AtomicBool::new(false),
            &ModelProfile::default(),
            &mut last_error,
        );
        assert_eq!(recovered, 0, "请求失败时补回条数为 0");
        assert!(segs[0].translation.is_none());
        let reason = last_error.expect("折半补译失败必须记下失败原因");
        assert!(reason.contains("400"), "应保留服务端的失败原因: {reason}");
    }

    /// 整批软失败（重试用尽）时这一批**不得**计入已完成条数：否则进度条先冲到 100%，
    /// 收尾/重试时再回落，看起来像「翻译完了又重新开始」。
    #[test]
    fn soft_failed_batch_is_not_counted_as_completed() {
        // 500 属可重试错误：喂满 3 次重试后整批软失败。
        let (addr, _seen) = spawn_mock_server_status(vec![
            (500, "boom".to_string()),
            (500, "boom".to_string()),
            (500, "boom".to_string()),
        ]);
        let mut cfg = online_cfg_for(&addr);
        cfg.batch_size = 2;
        let segs = vec![
            Segment::new(1, 0.0, 1.0, "你好"),
            Segment::new(2, 1.0, 2.0, "世界"),
        ];
        let seen_progress = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
        let recorder = seen_progress.clone();
        let cb: crate::engines::TextProgressCb = Box::new(move |p, _msg| {
            recorder.lock().unwrap().push(p);
        });
        // 整批都没译出 → 收尾按「未能译出任何一句」报错；这里只关心进度回调。
        let _ = translate_via_online_api(
            &cfg,
            segs,
            "English",
            Some(cb),
            Arc::new(AtomicBool::new(false)),
        );

        let seen = seen_progress.lock().unwrap().clone();
        assert!(!seen.is_empty(), "至少应有一次进度回调");
        assert!(
            seen.iter().all(|&p| p < 1.0),
            "软失败批次不得计入完成，进度不该到 100%: {seen:?}"
        );
    }
}

/// 在线翻译链路的健壮性回归：只测**纯函数**，不起子进程、不联网。
///
/// 与上面的 `tests` 模块分开：这里刻意**不用** `use super::*`，逐项列出被测函数，
/// 免得测试跟着实现细节（私有常量、内部类型）一起漂移。
#[cfg(test)]
mod translate_robustness_tests {
    use super::{api_key_header_error, ensure_target_lang, truncation_victim, wait_with_cancel};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    /// 目标语言归一化：首尾空白必须去掉（否则写出的语言标签与界面/增量判定的精确
    /// 比对对不上，界面永远显示「未译」，每点一次「开始翻译」都整片重译）；
    /// 空值/纯空白必须直接报错，而不是让提示词退化成「翻译为地道的。」并写回
    /// 恒真的空语言标签（此后 `translation_matches("")` 永远为真）。
    #[test]
    fn target_lang_is_trimmed_and_blank_is_rejected() {
        assert_eq!(ensure_target_lang("  English  ").expect("非空"), "English");
        assert_eq!(ensure_target_lang("简体中文").expect("非空"), "简体中文");
        assert_eq!(ensure_target_lang("日本語\n").expect("非空"), "日本語");

        for blank in ["", "   ", "\t\n"] {
            let err = ensure_target_lang(blank).expect_err("空目标语言必须报错");
            assert!(err.to_string().contains("目标语言"), "{err}");
        }
    }

    /// 截断受害者必须按「译文在响应体里最后出现的位置」挑，而不是按批内顺序：
    /// 模型乱序输出（先写 [3]、再写被切一半的 [2]）时，按批内顺序取最后一条会丢掉
    /// 完整的 [3]，却把半截的 [2] 当成完整译文写进字幕——静默截断。
    #[test]
    fn truncation_victim_follows_response_order_not_batch_order() {
        let content = "[1] 甲甲甲甲\n[3] 丙丙丙丙\n[2] 乙乙乙";
        let parsed = HashMap::from([
            (1usize, "甲甲甲甲".to_string()),
            (3usize, "丙丙丙丙".to_string()),
            (2usize, "乙乙乙".to_string()),
        ]);
        assert_eq!(
            truncation_victim(content, &[1, 2, 3], &parsed),
            Some(2),
            "被截断的是最后写下的 [2]，不能丢掉完整的 [3]"
        );

        // 正序输出：结论与旧行为一致（丢最后一条）
        let in_order = "[1] aaa\n[2] bbb";
        let parsed_ok = HashMap::from([(1usize, "aaa".to_string()), (2usize, "bbb".to_string())]);
        assert_eq!(truncation_victim(in_order, &[1, 2], &parsed_ok), Some(2));

        // 译文文本在响应体里定位不到：回退到批内倒序的第一条
        let unlocatable = HashMap::from([(1usize, "不在响应体里".to_string())]);
        assert_eq!(truncation_victim("", &[1, 2], &unlocatable), Some(1));

        // 一条都没解析出来：没有受害者
        assert_eq!(truncation_victim("", &[1, 2], &HashMap::new()), None);
    }

    /// `Authorization` 头只接受可见 ASCII：密钥里的换行/全角字符必须在发请求前被
    /// 拦下，否则底层客户端报「非法头」，还会被误判成可重试的「无法连接」，
    /// 重试 3 次后只报「未能译出任何一句」，把真正的根因藏起来。
    #[test]
    fn api_key_header_chars_are_validated_before_request() {
        assert!(api_key_header_error("sk-abc_123-_.~+/=").is_none());
        assert!(
            api_key_header_error("  sk-abc\n").is_none(),
            "首尾空白必须被容忍（复制密钥最常见的尾随换行）"
        );

        let newline = api_key_header_error("sk-abc\ndef").expect("内嵌换行必须被拒绝");
        assert!(newline.contains("API Key"), "{newline}");
        assert!(newline.contains("U+000A"), "应指出非法字符码位: {newline}");
        assert!(newline.contains("第 7 个字符"), "应指出位置: {newline}");

        let full_width = api_key_header_error("sk-密钥").expect("非 ASCII 必须被拒绝");
        assert!(full_width.contains("U+"), "{full_width}");
    }

    /// 退避等待必须可取消：已取消时立刻返回 `false`，而不是把 0.6~1.8s 睡完
    /// （睡醒还会再发一次请求）。
    #[test]
    fn backoff_wait_is_interruptible() {
        let idle = AtomicBool::new(false);
        assert!(
            wait_with_cancel(Duration::from_millis(1), &idle),
            "未取消时应等待完再返回 true"
        );

        let cancelled = AtomicBool::new(true);
        let started = Instant::now();
        assert!(
            !wait_with_cancel(Duration::from_secs(5), &cancelled),
            "已取消时必须返回 false"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "取消后不该把整段退避睡完"
        );
    }
}
