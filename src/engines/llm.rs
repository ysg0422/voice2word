//! LLM 引擎 — 基于 llama.cpp 的字幕文本润色 (标点恢复、错别字修正、智能断句)

use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::subtitle::Segment;
use crate::utils::TempPathGuard;

// 每批最多 24 条字幕：条数是「模型加载 / 请求次数」的节流阀，批量越大越快。
const MAX_SEGMENTS_PER_BATCH: usize = 24;

/// 输入侧 token/字符 经验系数。
///
/// 实测 Qwen2.5 分词器：中文字幕约 0.72~0.80 token/字符。取 0.85 留余量，
/// 避免英文混排、数字、符号偏多时低估输入长度。
const PROMPT_TOKENS_PER_CHAR: f64 = 0.85;
/// 输出侧 token/字符 经验系数。
///
/// 译文长度可能**超过**原文（中→英实测约 0.84 token/字符），按 1.0 封顶，
/// 给膨胀型目标语言留足空间。
const OUTPUT_TOKENS_PER_CHAR: f64 = 1.0;
/// 上下文里留给「对话模板 + 特殊 token」的固定余量。
const CTX_MARGIN: u32 = 192;

/// 按上下文窗口推导「一批源文本的字符预算」。
///
/// 一批要同时装下「固定开销（系统提示词模板 + 术语表 + 源语言线索 + 对话模板）」
/// 「输入 prompt」与「模型输出」，后两者都随源字符数线性增长，因此
/// 预算 = (ctx - 固定开销 - 余量) / (输入系数 + 输出系数)。
///
/// `fixed_tokens` 必须包含**本次**系统提示词与术语表的实际长度：术语表理论上限
/// 是单条左右各 60 字符 × 80 条 ≈ 9.7k 字符（≈8k+ token），已是 4096 上下文的
/// 两倍，而且**每批重复注入**。此前只扣 `CTX_MARGIN`（192），术语表一长就撑爆
/// 上下文——表现为译文被截断、整批只解析出前几条。
fn batch_char_budget(ctx_size: u32, fixed_tokens: u32) -> usize {
    let usable = ctx_size
        .saturating_sub(CTX_MARGIN)
        .saturating_sub(fixed_tokens) as f64;
    let per_char = PROMPT_TOKENS_PER_CHAR + OUTPUT_TOKENS_PER_CHAR;
    ((usable / per_char) as usize).max(64)
}

/// 一批译文的输出 token 上限（`-n` / `n_predict`）。
///
/// 此前按「条数 × 48」估算并夹在 768：那是 8 条/批时代的经验值，假定每行都短。
/// 而批次实际按**字符**预算切分，长行批次（如 24 行 × 150 字）需要上千 token
/// 才能译完，768 会**静默截断**——实测 24 行只译出 14 行（模型撞上 767 步上限），
/// 剩下 10 行原样留空，用户看到的是「翻译莫名其妙少了几句」。
///
/// 改为按源字符数给预算，再把「固定开销 + 本批输入」从上下文里扣掉，剩下的才是
/// 能给输出的空间（同样保证 prompt + 输出不越界）。
fn output_token_budget(source_chars: usize, ctx_size: u32, fixed_tokens: u32) -> u32 {
    let want = (source_chars as f64 * OUTPUT_TOKENS_PER_CHAR) as u32;
    let prompt_est = (source_chars as f64 * PROMPT_TOKENS_PER_CHAR) as u32 + CTX_MARGIN;
    let remaining = ctx_size
        .saturating_sub(prompt_est)
        .saturating_sub(fixed_tokens)
        .saturating_sub(64);
    want.max(128).min(remaining.max(128))
}

/// 把「系统提示词 + 术语表 + 源语言线索」的实际长度折算成 token 数。
///
/// 按 [`PROMPT_TOKENS_PER_CHAR`] 折算并向上取整：宁可高估（少装几条字幕），也不要
/// 低估（撑爆上下文，导致整批译文被截断且无法察觉）。
fn fixed_prompt_tokens(fixed_prompt: &str) -> u32 {
    (fixed_prompt.chars().count() as f64 * PROMPT_TOKENS_PER_CHAR).ceil() as u32
}

/// 「译文 == 原文」的复制率告警阈值（P0-B 兜底）。
const COPY_RATE_ALERT: f64 = 0.5;
/// 复制率样本下限：待译句太少时比例没有统计意义（3 句里 2 句是复制不代表模型摆烂）。
const COPY_RATE_MIN_SAMPLES: usize = 4;

/// 翻译提示词里「固定套话 + 源语言线索」的 token 折算值（术语表长度另行实测）。
///
/// 折算自 `translate_batch` 的系统提示词模板（约 110 个汉字 × 0.85 ≈ 94）与
/// 源语言线索（`源语言为中文；` 约 7 字 ≈ 6），向上取整。
const PROMPT_HINT_TOKENS: u32 = 128;
/// 润色提示词的固定开销（`polish_batch` 的系统提示词模板约 120 汉字 ≈ 102）。
const POLISH_FIXED_PROMPT_TOKENS: u32 = 128;

/// 「译文 == 原文」的统计口径（P0-B）。
///
/// 逐句判据（[`is_untranslated_copy`]）会把复制原文的句子当成「未译出」并走折半
/// 重试；但若**整篇过半**都是复制，说明模型在整体摆烂——必须在收尾处显式告警或
/// 报错，不能静默返回 Ok 让界面显示「翻译已完成」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CopyRate {
    /// 被判为「复制原文」而拒收的句子数
    pub rejected: usize,
    /// 真正送去翻译的待译句数（分母）
    pub checked: usize,
}

impl CopyRate {
    pub(crate) fn rate(self) -> f64 {
        if self.checked == 0 {
            0.0
        } else {
            self.rejected as f64 / self.checked as f64
        }
    }

    /// 是否达到「疑似整篇摆烂」的告警阈值。
    pub(crate) fn is_alarming(self) -> bool {
        self.checked >= COPY_RATE_MIN_SAMPLES && self.rate() > COPY_RATE_ALERT
    }

    /// 收尾时给用户的提示文案（同时用于日志与进度回调）。
    pub(crate) fn warning_message(self) -> String {
        format!(
            "⚠ 检出 {}/{} 句未真正译出（模型疑似复制原文），可再次点击「开始翻译」补译",
            self.rejected, self.checked
        )
    }
}

/// 是否是 CJK 统一表意文字（含扩展 A/B 与兼容区）。用于复制判据的「短句/非中文放行」。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
}

/// 复制判据用到的归一化：去掉**所有**空白字符 + 统一小写。
///
/// 中文标点与全/半角**不做**归一：全角与半角数字/标点混用恰恰是「只做了字符
/// 搬运、没真翻译」的信号之一，抹平它们反而会放过真正的复制；反过来，保留它们
/// 也不会误杀——真正的译文几乎不可能与原文连标点都一字不差。
fn normalize_for_copy_check(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 「译文 == 原文」是否说明**该句没译出**（小模型摆烂时直接复制原文）。
///
/// 设计约束（避免误杀）：有些句子「译文 == 原文」本来就是**正确**的——
/// 纯数字（`1.3`）、术语、人名、`OK`、公式（`P(A+B)`）。因此放行两类：
/// - 原文 < 4 个字符：短到不可能有有意义的翻译；
/// - 原文不含任何 CJK 文字：纯公式/纯拉丁术语，本就该原样保留。
///
/// 只有「含中文的长句却被一字不差地吐回来」才判为复制——这正是要抓的场景。
///
/// 另外，目标是中文变体（简体/繁体中文）时调用方**不走**本判据：中文变体已由
/// zhconv 确定性转换处理，「简→简」本就等于原文，不能算失败。
pub(crate) fn is_untranslated_copy(source: &str, translation: &str) -> bool {
    if source.chars().count() < 4 || !source.chars().any(is_cjk) {
        return false;
    }
    normalize_for_copy_check(source) == normalize_for_copy_check(translation)
}

/// 目标语言是否是中文变体；是则返回对应的 zhconv 目标变体。
///
/// 中文变体之间的转换是**确定性字符映射**，不需要（也不该）交给小模型：源字幕本来
/// 就是中文时，提示词会变成自相矛盾的「源语言为中文；……翻译为地道的繁体中文。」，
/// 小模型最省力的输出就是**原样复制**——实测 8/8 句与原文一模一样，界面却显示
/// 「翻译已完成（8 句）」。这里改用 `zhconv` 直接转换（用法同
/// `whisper::normalize_zh_text`）。
pub(crate) fn chinese_variant_target(target_lang: &str) -> Option<zhconv::Variant> {
    let t = target_lang.trim();
    let lower = t.to_ascii_lowercase();
    if t.contains('繁') || lower.starts_with("zh-hant") || lower.starts_with("zh-tw") {
        Some(zhconv::Variant::ZhHant)
    } else if t.contains('简')
        || lower.starts_with("zh-hans")
        || lower == "zh"
        || lower.starts_with("zh-cn")
    {
        Some(zhconv::Variant::ZhHans)
    } else {
        None
    }
}

/// 中文变体目标（简→繁 / 繁→简）的**确定性转换**，替代 LLM 翻译（P0-A）。
///
/// 与 LLM 路径保持**一致的下游契约**：同样写入 `translation` 与 `translation_lang`，
/// 因此 `has_translation` / `translation_matches` / 增量判定照常工作。仍然发进度回调
/// 并尊重取消标志：zhconv 单次转换在毫秒级，但整片可能有上千句，逐句检查一次取消，
/// 既让「终止」及时生效，也让进度条有反馈。
pub(crate) fn convert_chinese_variant(
    mut segments: Vec<Segment>,
    target_lang: &str,
    variant: zhconv::Variant,
    progress_cb: Option<crate::engines::TextProgressCb>,
    cancel: &AtomicBool,
) -> Result<Vec<Segment>> {
    use std::sync::atomic::Ordering;

    let total = segments.len();
    info!(
        "中文变体目标「{}」走 zhconv 确定性转换（不调用 LLM），共 {} 句",
        target_lang, total
    );
    if let Some(ref cb) = progress_cb {
        cb(0.0, &format!("正在转换为{target_lang}（本地字符映射）..."));
    }

    // 增量语义与 LLM 路径保持一致：仅转换「还没有目标语言译文」的句子。
    let pending: Vec<usize> = segments
        .iter()
        .enumerate()
        .filter(|(_, seg)| {
            !seg.translate_source().trim().is_empty() && !seg.translation_matches(target_lang)
        })
        .map(|(pos, _)| pos)
        .collect();
    let pending_total = pending.len();
    if pending_total == 0 {
        info!("所有片段均已是 {target_lang} 译文，无需转换");
        if let Some(ref cb) = progress_cb {
            cb(1.0, &format!("全部 {total} 句已是{target_lang}译文"));
        }
        return Ok(segments);
    }

    let mut completed = 0usize;
    for pos in pending {
        if cancel.load(Ordering::Relaxed) {
            info!("中文变体转换被取消，保留已完成的部分（{completed}/{pending_total}）");
            break;
        }
        let converted = zhconv::zhconv(&segments[pos].translate_source(), variant);
        segments[pos].translation = Some(converted);
        segments[pos].translation_lang = Some(target_lang.to_string());
        completed += 1;
        // 每 32 句回报一次：太长片的进度条不至于长时间不动。
        if completed.is_multiple_of(32) {
            if let Some(ref cb) = progress_cb {
                let progress = completed as f64 / pending_total.max(1) as f64;
                cb(
                    progress,
                    &format!("正在转换为{target_lang}: {completed}/{pending_total} 句"),
                );
            }
        }
    }
    if let Some(ref cb) = progress_cb {
        let progress = completed as f64 / pending_total.max(1) as f64;
        cb(
            progress,
            &format!("{target_lang}转换完成: {completed}/{pending_total} 句"),
        );
    }
    info!("中文变体转换完成（{completed}/{pending_total}）");
    Ok(segments)
}

struct LlamaServer {
    child: Child,
    address: String,
}

impl LlamaServer {
    fn start(server_path: &Path, model_path: &Path, ctx_size: u32, threads: u32) -> Option<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);

        let mut cmd = Command::new(server_path);
        // 让路模式：llama-server 是常驻进程，CPU 占用高，必须跟随全局设置
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        let child = cmd
            .arg("-m")
            .arg(model_path)
            .arg("-c")
            .arg(ctx_size.to_string())
            .arg("-t")
            .arg(threads.to_string())
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("-np")
            .arg("1")
            .arg("--no-warmup")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        crate::utils::child_registry::adopt(&child);

        let address = format!("127.0.0.1:{port}");
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(30) {
            if TcpStream::connect_timeout(&address.parse().ok()?, Duration::from_millis(250))
                .is_ok()
            {
                return Some(Self { child, address });
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        None
    }

    fn complete(&mut self, prompt: &str, max_tokens: u32) -> Option<CompletionOutcome> {
        let body = serde_json::json!({
            "prompt": prompt,
            "n_predict": max_tokens,
            "temperature": 0.01,  // 降低温度加速生成，提高确定性
            "stop": ["<|im_end|>"],
            "cache_prompt": true,
        })
        .to_string();
        let mut stream =
            TcpStream::connect_timeout(&self.address.parse().ok()?, Duration::from_secs(2)).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .ok()?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .ok()?;
        let request = format!(
            "POST /completion HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.address,
            body.len(),
            body
        );
        stream.write_all(request.as_bytes()).ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        let (_, payload) = response.split_once("\r\n\r\n")?;
        let value = serde_json::from_str::<serde_json::Value>(payload).ok()?;
        Some(CompletionOutcome {
            content: value.get("content")?.as_str()?.to_owned(),
            // P1-D：llama.cpp 的 `/completion` 在撞上 `n_predict` 时会给出
            // `stop_type: "limit"`（实测），这是「因长度截断」的可靠信号。
            // `truncated` 是「prompt 被截断」的旧字段，语义不同，不能拿来当
            // 输出截断判据（实测正常响应里它恒为 false）。
            stopped_by_limit: llama_stop_type_is_limit(&value),
        })
    }
}

/// llama.cpp `/completion` 是否**因达到 `n_predict` 上限**而停止。
///
/// 实测原始响应（qwen2.5-0.5b，n_predict=12）：
/// `"stop":true, ..., "truncated":false, "stop_type":"limit"`
/// 正常收尾（模型自己输出结束符或命中 stop 词）则是 `"stop_type":"eos"`。
/// `stop_type` 缺失时返回 `false`（宁可当成没截断，也不要把正常响应误报成截断）。
fn llama_stop_type_is_limit(value: &serde_json::Value) -> bool {
    value
        .get("stop_type")
        .and_then(|t| t.as_str())
        .map(|t| t.eq_ignore_ascii_case("limit"))
        .unwrap_or(false)
}

/// 一次 `/completion` 的解析结果：正文 + 是否因长度被截断。
struct CompletionOutcome {
    content: String,
    stopped_by_limit: bool,
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// 把待译下标切成批次：**同时**受「条数」与「字符数」约束。
///
/// 原先只按条数切（24 条一批），一条长字幕就能把整批输入顶到模型上下文之外，
/// 表现为最后几条译文缺失（`parse_batch_response` 只认能解析出的行）。这里补上
/// 字符预算：一批的源文本总长超过 `char_budget` 就提前收口。
///
/// `char_budget` 由 [`batch_char_budget`] 按上下文窗口推导，不再硬编码——
/// 硬编码值在「长行 + 译文膨胀」时会越界，输出被截断且无法察觉。
///
/// 单条自身就超预算时仍单独成批——否则会陷入「永远切不出合法批次」的死循环，
/// 交回模型截断处理比在这里卡死更合理。
fn plan_translate_batches(
    pending: &[usize],
    segments: &[Segment],
    char_budget: usize,
) -> Vec<Vec<usize>> {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_chars = 0usize;

    for &pos in pending {
        // 序号文本（"[123] "）也算输入，粗估 6 字符
        let cost = segments[pos].translate_source().chars().count() + 6;
        let would_overflow = !current.is_empty()
            && (current.len() >= MAX_SEGMENTS_PER_BATCH || current_chars + cost > char_budget);
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

pub struct LLMEngine {
    cli_path: PathBuf,
    model_path: PathBuf,
    ctx_size: u32,
    /// 推理线程数：设置页可运行时调整，故用原子量而非普通字段
    threads: std::sync::atomic::AtomicU32,
    /// 取消标志。润色 / 翻译都是**分批长循环**，用户点「终止」后不能等整批跑完
    /// 才生效（长视频几百条字幕要等几分钟）。每个批次开始前检查一次，置位即返回。
    ///
    /// 用 `Mutex<Arc<..>>` 而不是裸 `Arc`：标志由**外部**（管线的 cancelled、
    /// 翻译面板的取消位）在任务开始时注入，而 `LLMEngine` 在 `Arc` 后面被共享、
    /// 拿不到 `&mut self`。每批读一次锁，开销可忽略。
    ///
    /// 与 `TaskPipeline::cancelled` 共用同一个 `Arc`：转写的取消与润色的取消是
    /// 同一件事，各自维护一份标志会导致「终止转写」只杀掉 ASR、润色照跑。
    cancel: Mutex<Arc<AtomicBool>>,
    /// 术语表提示（注入翻译提示词）。空串表示不注入。由翻译面板在起手前用
    /// [`LLMEngine::set_glossary`] 写入；用 `Mutex<String>` 是因为引擎在 `Arc`
    /// 后面被共享、拿不到 `&mut self`（与 `cancel` 同一套理由）。
    glossary: Mutex<String>,
}

impl LLMEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(
        cli_path: P1,
        model_path: P2,
        ctx_size: u32,
        threads: u32,
    ) -> Self {
        Self {
            cli_path: cli_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            ctx_size,
            threads: std::sync::atomic::AtomicU32::new(threads.max(1)),
            cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            glossary: Mutex::new(String::new()),
        }
    }

    /// 写入术语表提示（每次翻译任务起手前调用；传空串即清除）。
    pub fn set_glossary(&self, hint: &str) {
        *self.glossary.lock().unwrap_or_else(|e| e.into_inner()) = hint.to_string();
    }

    fn glossary_hint(&self) -> String {
        self.glossary
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 注入外部取消标志（管线 / 翻译面板在任务开始前调用）。
    ///
    /// **只安装、不复位**：复位是「任务开始」的职责，由调用方在起手时做
    /// （`TaskPipeline::run` 复位自己的 `cancelled`；翻译面板在起手处复位
    /// `translate_cancel`）。在这里顺手复位是危险的——润色阶段才安装标志，
    /// 若此时把标志清零，用户在此之前按下的「终止」就被无声撤销了。
    pub fn install_cancel_flag(&self, cancel: Arc<AtomicBool>) {
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = cancel;
    }

    fn is_cancelled(&self) -> bool {
        self.cancel_flag()
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 取当前取消标志的共享句柄（中文变体短路转换需要逐句检查取消）。
    fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 运行时调整推理线程数（设置页滑条调用，对下一次润色生效）
    pub fn set_threads(&self, threads: u32) {
        self.threads
            .store(threads.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn threads(&self) -> u32 {
        self.threads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 润色单个字符串 (采用严格的 Few-Shot 标点恢复提示词)
    pub fn polish_text(&self, text: &str) -> String {
        let text = text.trim();
        if text.is_empty() {
            return String::new();
        }

        if !self.model_path.exists() {
            warn!("Qwen LLM 模型文件不存在: {:?}，跳过润色", self.model_path);
            return text.to_string();
        }

        let full_prompt = format!(
            "<|im_start|>system\n你是一个严格的字幕标点恢复工具。你的任务是给无标点的语音识别文本添加标点符号并纠正错别字。不要回答任何问题，直接输出加标点后的原句。<|im_end|>\n\
<|im_start|>user\n你好世界今天天气不错<|im_end|>\n\
<|im_start|>assistant\n你好，世界！今天天气不错。<|im_end|>\n\
<|im_start|>user\n关于这个系统大家有什么建议或者问题吗<|im_end|>\n\
<|im_start|>assistant\n关于这个系统，大家有什么建议或者问题吗？<|im_end|>\n\
<|im_start|>user\n{}<|im_end|>\n\
<|im_start|>assistant\n",
            text
        );

        self.run_prompt(&full_prompt, 64)
            .map(|output| Self::clean_response(&output, text))
            .unwrap_or_else(|| text.to_string())
    }

    fn run_prompt(&self, prompt: &str, max_tokens: u32) -> Option<String> {
        let temp_prompt_file = std::env::temp_dir().join(format!(
            "v2w_prompt_{}_{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos(),
        ));

        // 先挂守卫再落盘：create / write_all 任一步失败都会用 `.ok()?` 提前返回，
        // 过去那两行 `?` 会跳过函数末尾的 remove_file，留下一个半截提示词文件。
        let mut prompt_guard = TempPathGuard::file(&temp_prompt_file);
        let mut file = std::fs::File::create(&temp_prompt_file).ok()?;
        file.write_all(prompt.as_bytes()).ok()?;
        drop(file);

        let mut cli_cmd = Command::new(&self.cli_path);
        // 让路模式同样适用于命令行回退路径
        super::media_pipeline::apply_default_child_flags(&mut cli_cmd);
        let output = cli_cmd
            .arg("-m")
            .arg(&self.model_path)
            .arg("-c")
            .arg(self.ctx_size.to_string())
            .arg("-t")
            .arg(self.threads().to_string())
            .arg("-f")
            .arg(&temp_prompt_file)
            .arg("-n")
            .arg(max_tokens.to_string())
            .arg("--temp")
            .arg("0.01") // 降低温度加速生成
            .arg("-r")
            .arg("<|im_end|>")
            .arg("-no-cnv")
            .arg("--no-warmup")
            .arg("--no-display-prompt")
            .output();

        // 进程已退出，提示词文件不再需要：立刻删除（守卫仍会在返回时兜底）
        prompt_guard.remove_now();

        match output {
            Ok(out) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(out) => {
                warn!("llama.cpp 润色退出失败: {:?}", out.status.code());
                None
            }
            Err(e) => {
                warn!("调用 llama.cpp 润色失败: {}", e);
                None
            }
        }
    }

    fn start_server(&self) -> Option<LlamaServer> {
        let server_path = self.cli_path.with_file_name("llama-server.exe");
        if !server_path.exists() || !self.model_path.exists() {
            return None;
        }
        LlamaServer::start(
            &server_path,
            &self.model_path,
            self.ctx_size,
            self.threads(),
        )
    }

    /// 离线链路前置检查：模型与推理程序缺一不可。
    ///
    /// 为什么必须显式报错而不是「静默跑出 0 句」：`polish` / `translate` 走的是
    /// 「先起常驻服务、失败再回退命令行」两条路。模型文件缺失时两条路都起不来，
    /// 循环里每批都返回空映射，最后 `Ok(segments)` 原样返回——用户看到的是
    /// 「翻译已完成（0 句）」这种既没报错、也没任何结果的诡异状态，根本不知道
    /// 是「没下模型」。这里在开工前就给出可操作的中文提示，
    /// 两者都指向「性能设置 → 模型与组件」——模型与 llama.cpp 现在都可一键下载。
    fn ensure_ready(&self) -> Result<()> {
        if !self.model_path.exists() {
            return Err(anyhow!(
                "本地 Qwen 模型未就位：{}。请到「性能设置 → 模型与组件」点击下载，或改用「在线 API」翻译。",
                self.model_path.display()
            ));
        }
        // 命令行回退要用的主程序；常驻服务是它的同目录兄弟（llama-server.exe）。
        // llama.cpp 现在在下载清单里（同一个 zip 会把
        // llama-completion.exe / llama-server.exe 与各 DLL 一并解压到 tools/），
        // 因此提示只指向界面下载即可，不再要求用户手动放文件。
        if !self.cli_path.exists() {
            return Err(anyhow!(
                "本地推理程序未就位：{}。请到「性能设置 → 模型与组件」下载「llama.cpp 推理程序」（会自动解压到 tools/）；若你把它装在别处，也可在 config.toml 的 paths.llama_cli 指向实际位置，或改用「在线 API」翻译。",
                self.cli_path.display()
            ));
        }
        Ok(())
    }

    /// 批量润色字幕片段
    pub fn polish(
        &self,
        mut segments: Vec<Segment>,
        progress_cb: Option<crate::engines::TextProgressCb>,
    ) -> Result<Vec<Segment>> {
        let total = segments.len();
        // 先做前置检查：模型/程序缺失时立刻给出可操作的中文提示，
        // 而不是跑完一圈「0 句」让用户以为翻译成功了。
        self.ensure_ready()?;
        info!("LLM 开始润色 {} 个字幕片段", total);
        if let Some(ref cb) = progress_cb {
            cb(0.0, "正在加载 Qwen 模型，首次启动通常需要几秒...");
        }
        let mut server = self.start_server();
        if server.is_some() {
            info!("Qwen 常驻服务已就绪，将复用已加载模型完成批量润色");
        } else {
            warn!("Qwen 常驻服务不可用，回退到命令行批量润色");
        }

        let mut completed = 0usize;
        for group in segments.chunks_mut(MAX_SEGMENTS_PER_BATCH) {
            if self.is_cancelled() {
                info!("润色被取消，保留已完成的部分（{completed}/{total}）");
                break;
            }
            let group_len = group.len();
            // 长字幕按**字符预算**进一步拆开：预算随上下文窗口推导（见
            // `batch_char_budget`），不再靠「超过阈值就把批数减半」这种粗估。
            // 润色链路没有术语表/源语言线索，固定开销只有系统提示词模板
            // （`polish_batch` 里的那段），按实际长度折算了约 104 token。
            let budget = batch_char_budget(self.ctx_size, POLISH_FIXED_PROMPT_TOKENS);
            let mut start = 0usize;
            while start < group.len() {
                if self.is_cancelled() {
                    break;
                }
                let mut end = start;
                let mut chars = 0usize;
                while end < group.len() {
                    let cost = group[end].text.chars().count() + 6;
                    if end > start && chars + cost > budget {
                        break;
                    }
                    chars += cost;
                    end += 1;
                }
                let results = self.polish_batch(&group[start..end], server.as_mut());
                for seg in group[start..end].iter_mut() {
                    seg.polished = results
                        .get(&seg.index)
                        .cloned()
                        .unwrap_or_else(|| seg.text.clone());
                }
                start = end;
            }

            completed += group_len;
            if let Some(ref cb) = progress_cb {
                let progress = completed as f64 / total.max(1) as f64;
                cb(
                    progress,
                    &format!("Qwen 批量润色中: {}/{}", completed, total),
                );
            }
        }

        info!("LLM 全部片段润色完成");
        Ok(segments)
    }

    /// 批量翻译字幕片段（从外语/多语言翻译为目标语言，例如简体中文）
    pub fn translate(
        &self,
        mut segments: Vec<Segment>,
        target_lang: &str,
        progress_cb: Option<crate::engines::TextProgressCb>,
    ) -> Result<Vec<Segment>> {
        let total = segments.len();
        // P0-A：目标语言是中文变体（简体/繁体中文）时**不调用 LLM**——源字幕本来就是
        // 中文，提示词会自相矛盾，小模型只会原样复制（实测 8/8 句与原文相同，界面却
        // 显示「翻译已完成」）。中文变体之间是确定性字符映射，用 zhconv 直接转换。
        //
        // 这里也做一次判定（不只是引擎构造处的 `TranslateEngine`），因为
        // `LLMEngine::translate` 是公开入口：短路判定必须落在最终执行的地方。
        if let Some(variant) = chinese_variant_target(target_lang) {
            return convert_chinese_variant(
                segments,
                target_lang,
                variant,
                progress_cb,
                &self.cancel_flag(),
            );
        }
        // 同 polish：模型/程序缺失时 fail-fast，避免「翻译完成 0 句」的假成功。
        self.ensure_ready()?;
        info!("LLM 开始将 {} 个字幕片段翻译为 {}", total, target_lang);
        if let Some(ref cb) = progress_cb {
            cb(0.0, "正在加载 Qwen 翻译模型，首次启动需要几秒...");
        }
        let mut server = self.start_server();
        if server.is_some() {
            info!("Qwen 常驻服务已就绪，复用模型进行批量字幕翻译");
        } else {
            warn!("Qwen 常驻服务不可用，回退到命令行批量翻译");
        }

        // 增量：只译「还没有目标语言译文」的句子。
        //
        // 翻译是可以重入的——中途取消、换目标语言、或补译后来新增的句子，
        // 都不该把已经译好的部分再烧一遍。判定用 `translation_matches` 精确比对
        // 目标语言，因此「从 English 改译 日本語」会正确地整篇重译，
        // 而「上次取消了、这次继续」只补剩下的。
        let pending: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter(|(_, seg)| {
                !seg.translate_source().trim().is_empty() && !seg.translation_matches(target_lang)
            })
            .map(|(pos, _)| pos)
            .collect();

        let pending_total = pending.len();
        if pending_total == 0 {
            info!("所有片段均已是 {target_lang} 译文，无需翻译");
            if let Some(ref cb) = progress_cb {
                cb(1.0, &format!("全部 {total} 句已是{target_lang}译文"));
            }
            return Ok(segments);
        }
        if pending_total < total {
            info!("增量翻译：{total} 句中有 {pending_total} 句需要翻译为 {target_lang}");
        }

        // 术语表 + 源语言线索是**每批重复注入**的固定开销：预算必须显式扣掉它们的
        // 真实长度，否则术语表一长就把上下文撑爆（见 `batch_char_budget`）。
        let glossary = self.glossary_hint();
        let fixed_tokens = fixed_prompt_tokens(&glossary) + PROMPT_HINT_TOKENS;
        if fixed_tokens > 0 {
            info!(
                "翻译提示词固定开销约 {} token（术语表 {} 字符），已从批量预算中扣除",
                fixed_tokens,
                glossary.chars().count()
            );
        }
        let budget = batch_char_budget(self.ctx_size, fixed_tokens);
        let batches = plan_translate_batches(&pending, &segments, budget);
        let mut completed = 0usize;
        // P0-B：整批的「译文 == 原文」复制率统计
        let mut copy = CopyRate::default();
        for chunk in batches {
            if self.is_cancelled() {
                info!("翻译被取消，保留已完成的部分（{completed}/{pending_total}）");
                break;
            }
            // 克隆这一小批（≤24 条）交给批量函数：`translate_batch` 要的是
            // `&[Segment]`，而这里必须按下标写回原数组，克隆是唯一不用和借用
            // 检查器搏斗的写法。相比一次模型推理，这点拷贝可忽略。
            let batch: Vec<Segment> = chunk.iter().map(|&pos| segments[pos].clone()).collect();
            let results = self.translate_batch(&batch, target_lang, &glossary, server.as_mut());
            let mut missed: Vec<usize> = Vec::new();
            // 被判为「复制原文」的句子：折半重试**之后**仍未译出才计入复制率，
            // 否则「重试补回来」的好句子会被误算成复制，把复制率抬高。
            let mut copy_candidates: Vec<usize> = Vec::new();
            copy.checked += chunk.len();
            for &pos in &chunk {
                match results.get(&segments[pos].index) {
                    Some(trans) => {
                        // P0-B：小模型「整体摆烂」时会一字不差地复制原文，绝不能当成
                        // 成功译文写回——那正是「界面显示翻译完成、实际什么都没翻」的
                        // 元凶。判为复制就改走**已有的折半重试**路径（小批更不容易
                        // 摆烂）；重试仍复制则保持未译，由上层显式报「未译出」。
                        if is_untranslated_copy(&segments[pos].translate_source(), trans) {
                            copy_candidates.push(pos);
                            missed.push(pos);
                        } else {
                            segments[pos].translation = Some(trans.clone());
                            segments[pos].translation_lang = Some(target_lang.to_string());
                        }
                    }
                    // 本批没译出这一句：收集起来，稍后**对半重试**。
                    None => missed.push(pos),
                }
            }

            // 模型偶发漏行（输出截断、行格式串味）时，整批丢弃会让用户看到
            // 「翻译莫名其妙少了几句」。把漏掉的句子按更小的批（对半）再试一轮，
            // 小批更不容易撞上输出长度上限，通常一轮就能补齐。
            if !missed.is_empty() {
                self.retry_missing(
                    &mut segments,
                    &missed,
                    target_lang,
                    &glossary,
                    server.as_mut(),
                );
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
                    &format!("Qwen 批量翻译中: {completed}/{pending_total} 句（共 {total} 句）"),
                );
            }
        }

        // 一句都没译出、又没被取消：多半是模型/程序异常（常驻服务起不来、命令行回退
        // 也失败）。此时 `Ok` 会让上层显示「翻译已完成（0 句）」——彻头彻尾的假成功，
        // 用户以为译完了、实际全空。这里改成显式报错，让失败在界面上看得见。
        if !self.is_cancelled() {
            let ok = pending
                .iter()
                .filter(|&&pos| segments[pos].translation_matches(target_lang))
                .count();
            if ok == 0 {
                return Err(anyhow!(
                    "Qwen 未能译出任何一句（共 {pending_total} 句待译）。请检查「模型与组件」里的 Qwen 模型与 llama.cpp 是否就位，或改用「在线 API」翻译。"
                ));
            }
            // P0-B 兜底：复制率过半说明模型整体在复制原文（而不是个别句子难译）。
            // 这时**不能静默返回 Ok**——界面会显示「翻译已完成（N 句）」的假成功。
            // 按原有「未译出即报错」的约定，整体复制率过高时显式报错，让失败可见。
            if copy.is_alarming() {
                warn!(
                    rejected = copy.rejected,
                    checked = copy.checked,
                    "Qwen 翻译复制率过高（{:.0}%）：译文与原文相同，疑似模型未真正翻译",
                    copy.rate() * 100.0
                );
                if let Some(ref cb) = progress_cb {
                    cb(1.0, &copy.warning_message());
                }
                return Err(anyhow!(
                    "Qwen 翻译质量异常：{}/{} 句译文与原文完全相同（复制率 {:.0}%），已按「未译出」处理。建议改用「在线 API」翻译，或换用更大的本地模型。",
                    copy.rejected,
                    copy.checked,
                    copy.rate() * 100.0
                ));
            }
            if copy.rejected > 0 {
                warn!(
                    rejected = copy.rejected,
                    checked = copy.checked,
                    "Qwen 有 {} 句译文与原文相同，已判为未译出",
                    copy.rejected
                );
            }
        }
        info!("LLM 全部字幕翻译完成");
        Ok(segments)
    }

    /// 把上一轮**没译出**的句子按更小的批重试一轮，直到补全或批次缩到单句为止。
    ///
    /// 为什么要专门做这件事：模型输出被截断 / 行格式串味时，`translate_batch` 只
    /// 返回能解析出的行，剩下的**静默丢失**——用户看到的是「翻译莫名其妙少了几句」，
    /// 而且没有任何提示。这里把漏掉的句子折半再问一次：批越小，输出越不容易撞上
    /// 长度上限，一轮下来通常就补齐了；实在补不上的（如某个序号模型就是吐不出）
    /// 才放弃，并留一条日志。
    fn retry_missing(
        &self,
        segments: &mut [Segment],
        missed: &[usize],
        target_lang: &str,
        glossary: &str,
        mut server: Option<&mut LlamaServer>,
    ) {
        let mut todo: Vec<usize> = missed.to_vec();
        // 每轮把待补清单**对半**拆成更小的批（单条时 half=1，即「单句重试一次」）。
        // `guard` 只防极端情况下的死循环；正常 24 条最多 5 轮就收敛。
        let mut guard = 0;
        while !todo.is_empty() && guard < 8 {
            guard += 1;
            let half = todo.len().div_ceil(2);
            let mut still_missing: Vec<usize> = Vec::new();
            for sub in todo.chunks(half) {
                if self.is_cancelled() {
                    return;
                }
                let batch: Vec<Segment> = sub.iter().map(|&p| segments[p].clone()).collect();
                let results =
                    self.translate_batch(&batch, target_lang, glossary, server.as_deref_mut());
                for &pos in sub {
                    match results.get(&segments[pos].index) {
                        Some(trans) => {
                            // 折半后仍原样复制 → 就是没译出：保持未译，交给上层判定。
                            if is_untranslated_copy(&segments[pos].translate_source(), trans) {
                                still_missing.push(pos);
                            } else {
                                segments[pos].translation = Some(trans.clone());
                                segments[pos].translation_lang = Some(target_lang.to_string());
                            }
                        }
                        None => still_missing.push(pos),
                    }
                }
            }
            // 这一轮没有任何进展：模型对这几个序号就是不出，收手，免得空转。
            if still_missing.len() >= todo.len() {
                todo = still_missing;
                break;
            }
            todo = still_missing;
        }
        if !todo.is_empty() {
            warn!(
                count = todo.len(),
                "Qwen 翻译对 {} 句折半重试后仍未译出，保持未翻译（可再次点击「开始翻译」补译）",
                todo.len()
            );
        }
    }

    fn translate_batch(
        &self,
        segments: &[Segment],
        target_lang: &str,
        glossary: &str,
        server: Option<&mut LlamaServer>,
    ) -> std::collections::HashMap<usize, String> {
        let source_segments = segments
            .iter()
            .filter(|seg| !seg.translate_source().trim().is_empty())
            .collect::<Vec<_>>();
        if source_segments.is_empty() {
            return Default::default();
        }

        let source = source_segments
            .iter()
            .map(|seg| format!("[{}] {}", seg.index, seg.translate_source()))
            .collect::<Vec<_>>()
            .join("\n");
        // 源语言线索：ASR 已检测到整批的语言（Segment::language）。多给这一条，
        // 模型在「日→中」「中英混排」这类方向上更少漏译、更少误翻专名。
        let src_hint = crate::subtitle::dominant_language(source_segments.iter().copied())
            .map(|code| format!("源语言为{}；", crate::subtitle::language_name(&code)))
            .unwrap_or_default();
        let prompt = format!(
            "<|im_start|>system\n你是专业字幕翻译专家。{}{}把用户给出的带序号字幕逐条翻译为地道的{}，保持原意与语气，语言通顺紧凑；译文长度尽量与原文相当，避免明显长于原文。数字、公式、变量名、编号与专有名词原样保留，不得改写、不得意译。必须逐行输出，格式严格为「[序号] 译文」，不得解释、不得合并、不得遗漏、不得改动序号。<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
            src_hint,
            glossary,
            target_lang,
            source
        );
        let source_chars: usize = source_segments
            .iter()
            .map(|s| s.translate_source().chars().count())
            .sum();
        // 输出预算同样要扣掉「固定开销 + 本批输入」，否则 术语表 + 长批 会把
        // prompt + 输出顶出上下文窗口。
        let fixed_tokens =
            fixed_prompt_tokens(&format!("{src_hint}{glossary}")) + PROMPT_HINT_TOKENS;
        let max_tokens = output_token_budget(source_chars, self.ctx_size, fixed_tokens);
        // 命令行回退路径（`run_prompt`）拿不到 `stop_type`，截断信号只能置 false：
        // 只有常驻服务 `/completion` 的响应带 `stop_type`。
        let outcome = server
            .and_then(|server| server.complete(&prompt, max_tokens))
            .or_else(|| {
                self.run_prompt(&prompt, max_tokens)
                    .map(|content| CompletionOutcome {
                        content,
                        stopped_by_limit: false,
                    })
            });
        let Some(CompletionOutcome {
            content: output,
            stopped_by_limit,
        }) = outcome
        else {
            return Default::default();
        };

        let expected = source_segments
            .iter()
            .map(|seg| seg.index)
            .collect::<std::collections::HashSet<_>>();
        let mut result = Self::parse_batch_response(&output, &expected);
        // P1-D：输出被 `n_predict` 截断时，**最后一条几乎必然是被切一半的**（模型写到
        // 一半就撞上上限）。麻烦在于这种半截行仍能被 `parse_batch_response` 当成一条
        // 完整译文——上层「本批未译全」的分支不会触发，用户看到的就是一句残缺译文。
        // 与在线链路同一套策略：主动**丢掉最后一条已解析的译文**，用自带的「缺一条」
        // 触发折半重试（`retry_missing`）补回。
        if stopped_by_limit {
            let last_matched = source_segments
                .iter()
                .rev()
                .map(|s| s.index)
                .find(|i| result.contains_key(i));
            if let Some(idx) = last_matched {
                result.remove(&idx);
                warn!(
                    index = idx,
                    batch = source_segments.len(),
                    "Qwen 翻译输出被长度上限截断，丢弃末条并交由折半重试补齐"
                );
            }
        }
        if result.len() != source_segments.len() {
            warn!(
                "Qwen 翻译批量输出不完整 ({}/{} 条)，未匹配条目保持未翻译",
                result.len(),
                source_segments.len()
            );
        }
        result
    }

    /// 通过常驻服务处理一组字幕；服务故障时回退到命令行进程。
    fn polish_batch(
        &self,
        segments: &[Segment],
        server: Option<&mut LlamaServer>,
    ) -> std::collections::HashMap<usize, String> {
        let source_segments = segments
            .iter()
            .filter(|seg| !seg.text.trim().is_empty())
            .collect::<Vec<_>>();
        if source_segments.is_empty() {
            return Default::default();
        }

        let source = source_segments
            .iter()
            .map(|seg| format!("[{}] {}", seg.index, seg.text.replace(['\r', '\n'], " ")))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "<|im_start|>system\n你是严格的字幕润色工具。为每条字幕添加标点并修正错别字，保持原意。若文本为中文语境下误识别出的英文幻读（如数学公式读音被识为英文句子），请将其修正翻译为地道的简体中文。必须逐行输出，格式为 [序号] 润色文本；不解释，不合并，不遗漏。<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
            source
        );
        let source_chars: usize = source_segments.iter().map(|s| s.text.chars().count()).sum();
        let max_tokens =
            output_token_budget(source_chars, self.ctx_size, POLISH_FIXED_PROMPT_TOKENS);
        let output = server
            .and_then(|server| server.complete(&prompt, max_tokens))
            .or_else(|| {
                self.run_prompt(&prompt, max_tokens)
                    .map(|content| CompletionOutcome {
                        content,
                        stopped_by_limit: false,
                    })
            });
        let Some(output) = output else {
            return Default::default();
        };
        let output = output.content;

        let expected = source_segments
            .iter()
            .map(|seg| seg.index)
            .collect::<std::collections::HashSet<_>>();
        let result = Self::parse_batch_response(&output, &expected);
        if result.len() != source_segments.len() {
            warn!(
                "Qwen 批量输出不完整 ({}/{} 条)，未匹配条目将保留原文",
                result.len(),
                source_segments.len()
            );
        }
        result
    }

    /// 解析一行「序号 + 译文」，兼容模型常见的几种序号写法。
    ///
    /// 主格式是提示词要求的 `[序号] 译文`。但小模型（尤其 Qwen 0.5B）偶尔会写成
    /// `1. 译文` / `1) 译文` / `1、译文` / `1: 译文`——只认方括号的话，整行会被判为
    /// 解析失败而**丢失**（配合重试也只是反复失败）。这里按「前导数字 + 分隔符」
    /// 宽容识别；调用方再用 `expected` 集合过滤，因此译文正文里恰好以数字开头的
    /// 情况（如「2024 年」）不会误命中，除非该数字正好是某个待译序号。
    fn parse_indexed_line(line: &str) -> Option<(usize, String)> {
        let line = line.trim();
        // 主格式：[123] 译文
        if let Some(rest) = line.strip_prefix('[') {
            let (index, text) = rest.split_once(']')?;
            let index = index.trim().parse::<usize>().ok()?;
            let text = text.trim_start_matches([':', '：', '-', ' ']).trim();
            return Some((index, text.to_string()));
        }
        // 回退格式：123. / 123) / 123、/ 123: 后跟译文
        //
        // 必须紧跟一个**分隔标点**（不是纯空格）才算序号：否则「2 个苹果」这种
        // 恰好以数字开头的普通行会被误判成「序号 2 的译文」，一旦 2 正好是待译
        // 序号就会张冠李戴。宁可漏认（下一轮重试仍会补），不可错认。
        let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let rest_raw = &line[digits.len()..];
        let sep_ok = rest_raw
            .chars()
            .next()
            .map(|c| matches!(c, '.' | ')' | '）' | '、' | ':' | '：' | '-' | '—' | '|'))
            .unwrap_or(false);
        if !sep_ok {
            return None;
        }
        let index = digits.parse::<usize>().ok()?;
        let rest = rest_raw
            .trim_start_matches(['.', ')', '）', '、', ':', '：', '-', '—', '|', ' '])
            .trim();
        if rest.is_empty() {
            return None;
        }
        Some((index, rest.to_string()))
    }

    pub(crate) fn parse_batch_response(
        raw: &str,
        expected: &std::collections::HashSet<usize>,
    ) -> std::collections::HashMap<usize, String> {
        let mut result = std::collections::HashMap::new();
        for line in raw.lines() {
            let Some((index, text)) = Self::parse_indexed_line(line) else {
                continue;
            };
            let text = Self::strip_stop_tags(&text);
            if expected.contains(&index) && !text.is_empty() {
                result.insert(index, text);
            }
        }
        result
    }

    /// 清洗模型输出的多余标签与前后缀
    fn clean_response(raw: &str, original: &str) -> String {
        let mut clean = raw.trim().to_string();

        clean = Self::strip_stop_tags(&clean);

        if let Some(pos) = clean.find('\n') {
            clean = clean[..pos].trim().to_string();
        }

        let prefixes = [
            "修正后：",
            "修正后:",
            "润色后：",
            "润色后:",
            "结果：",
            "结果:",
            "输出：",
            "输出:",
        ];
        for p in prefixes {
            if clean.starts_with(p) {
                clean = clean[p.len()..].trim().to_string();
            }
        }

        if (clean.starts_with('"') && clean.ends_with('"'))
            || (clean.starts_with('“') && clean.ends_with('”'))
        {
            // 弯引号 “ ” 是多字节字符，按字节切片会 panic，必须用 strip 按字符边界剥壳
            if let Some(inner) = clean
                .strip_prefix('"')
                .or_else(|| clean.strip_prefix('“'))
                .and_then(|s| s.strip_suffix('"').or_else(|| s.strip_suffix('”')))
            {
                clean = inner.trim().to_string();
            }
        }

        if clean.is_empty() {
            original.to_string()
        } else {
            clean
        }
    }

    fn strip_stop_tags(text: &str) -> String {
        let mut clean = text.to_string();
        for stop_tag in &[
            "<|im_end|>",
            "<|endoftext|>",
            "[end of text]",
            "[end of text",
            "</s>",
        ] {
            if let Some(pos) = clean.to_ascii_lowercase().find(stop_tag) {
                clean = clean[..pos].trim().to_string();
            }
        }
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::{
        batch_char_budget, chinese_variant_target, fixed_prompt_tokens, is_untranslated_copy,
        llama_stop_type_is_limit, output_token_budget, plan_translate_batches, CopyRate, LLMEngine,
        MAX_SEGMENTS_PER_BATCH, OUTPUT_TOKENS_PER_CHAR, PROMPT_TOKENS_PER_CHAR,
    };
    use crate::subtitle::Segment;
    use std::collections::HashSet;

    #[test]
    fn parses_indexed_batch_response() {
        let expected = HashSet::from([1, 2]);
        let result = LLMEngine::parse_batch_response("[1] 第一条。\n[2] 第二条！", &expected);
        assert_eq!(result.get(&1).map(String::as_str), Some("第一条。"));
        assert_eq!(result.get(&2).map(String::as_str), Some("第二条！"));
    }

    /// 解析器必须宽容小模型常见的几种序号写法，否则整行会被判为失败而丢失。
    #[test]
    fn parses_tolerant_index_formats() {
        let expected = HashSet::from([1, 2, 3, 4, 5]);
        let raw = "[1] 方括号\n2. 点号\n3) 右括号\n4、顿号\n5：全角冒号";
        let result = LLMEngine::parse_batch_response(raw, &expected);
        assert_eq!(result.get(&1).map(String::as_str), Some("方括号"));
        assert_eq!(result.get(&2).map(String::as_str), Some("点号"));
        assert_eq!(result.get(&3).map(String::as_str), Some("右括号"));
        assert_eq!(result.get(&4).map(String::as_str), Some("顿号"));
        assert_eq!(result.get(&5).map(String::as_str), Some("全角冒号"));
    }

    /// 回退格式（`2. 译文`）必须带**分隔标点**：纯空格开头的「2 个苹果」不算序号，
    /// 否则会把它误当成序号 2 的译文，一旦 2 恰是待译序号就张冠李戴。
    #[test]
    fn fallback_index_requires_separator_punctuation() {
        assert_eq!(
            LLMEngine::parse_indexed_line("2. Hello"),
            Some((2, "Hello".to_string()))
        );
        assert_eq!(
            LLMEngine::parse_indexed_line("3、你好"),
            Some((3, "你好".to_string()))
        );
        assert_eq!(LLMEngine::parse_indexed_line("2 个苹果"), None);
        assert_eq!(LLMEngine::parse_indexed_line("Hello world"), None);
    }

    /// 宽容解析**不能**把译文正文里以数字开头的句子误当成序号。
    /// 靠 `expected` 集合过滤：正文里的数字几乎不会是「另一个待译序号」。
    #[test]
    fn tolerant_parse_does_not_steal_numbered_body() {
        let expected = HashSet::from([1]);
        // 第 1 句的译文正文恰好以「2024」开头——不该被当成序号 2024 的条目
        let raw = "[1] 2024 年是很特别的一年。";
        let result = LLMEngine::parse_batch_response(raw, &expected);
        assert_eq!(
            result.get(&1).map(String::as_str),
            Some("2024 年是很特别的一年。"),
            "正文里的数字不该被误切"
        );
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn strips_llama_stop_tags() {
        assert_eq!(LLMEngine::strip_stop_tags("结果。 [end of text]"), "结果。");
    }

    #[test]
    fn clean_response_strips_curly_quotes_without_panic() {
        // 回归：弯引号 “ ” 是多字节字符，按字节切片会 panic（not a char boundary）
        assert_eq!(LLMEngine::clean_response("“你好世界”", "原文"), "你好世界");
        assert_eq!(
            LLMEngine::clean_response("\"你好世界\"", "原文"),
            "你好世界"
        );
        // 纯中文内容超长时也不 panic
        let long_zh = "式".repeat(50);
        assert_eq!(
            LLMEngine::clean_response(&format!("“{long_zh}”"), "原文"),
            long_zh
        );
    }
    // ─────────── 翻译批次规划 ───────────

    fn seg_at(index: usize, text: &str) -> Segment {
        Segment::new(index, 0.0, 1.0, text)
    }

    /// 批次必须同时受条数约束。
    #[test]
    fn plan_batches_respects_max_segments() {
        let segs: Vec<Segment> = (1..=50).map(|i| seg_at(i, "短句")).collect();
        let pending: Vec<usize> = (0..50).collect();
        let batches = plan_translate_batches(&pending, &segs, batch_char_budget(4096, 0));
        assert!(
            batches.iter().all(|b| b.len() <= MAX_SEGMENTS_PER_BATCH),
            "每批不得超过 MAX_SEGMENTS_PER_BATCH"
        );
        // 所有下标都要被覆盖且不重复
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending);
    }

    /// 长句必须按**字符预算**提前收口——原先只按条数切，一条长字幕就能把整批
    /// 顶出模型上下文，表现为最后几行译文缺失。
    #[test]
    fn plan_batches_splits_on_char_budget() {
        let budget = batch_char_budget(4096, 0);
        let long = "字".repeat(500);
        let segs: Vec<Segment> = (1..=24).map(|i| seg_at(i, &long)).collect();
        let pending: Vec<usize> = (0..24).collect();
        let batches = plan_translate_batches(&pending, &segs, budget);
        assert!(batches.len() > 1, "长句应被拆成多批");
        for b in &batches {
            let chars: usize = b
                .iter()
                .map(|&p| segs[p].translate_source().chars().count() + 6)
                .sum();
            assert!(chars <= budget, "批次字符数超预算: {chars} > {budget}");
        }
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, pending, "不得丢句或重复");
    }

    /// 单条就超过字符预算时仍要单独成批：否则会切不出合法批次而死循环。
    #[test]
    fn plan_batches_keeps_oversized_single_segment() {
        let budget = batch_char_budget(4096, 0);
        let huge = "字".repeat(budget * 2);
        let segs = vec![seg_at(1, &huge)];
        let batches = plan_translate_batches(&[0], &segs, budget);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0]);
    }

    /// 空输入不该产出空批次。
    #[test]
    fn plan_batches_empty_input() {
        let segs: Vec<Segment> = Vec::new();
        assert!(plan_translate_batches(&[], &segs, batch_char_budget(4096, 0)).is_empty());
    }

    /// 回归：输出 token 预算必须**随批次源字符数增长**。
    ///
    /// 修前按「条数 × 48」夹在 768：24 行 × 150 字的批次实际需要 1300+ token 才能
    /// 译完，768 会在第 14 行撞上限后**静默截断**（实测复现），用户看到的是
    /// 「翻译莫名其妙少了几句」。这个断言锁死「长批不再被 768 卡死」。
    #[test]
    fn output_budget_scales_with_source_length() {
        let ctx = 4096;
        let short = output_token_budget(200, ctx, 0);
        let long = output_token_budget(3_000, ctx, 0);
        assert!(
            long > 768,
            "长批的输出预算必须突破旧的 768 上限，实际 {long}"
        );
        assert!(long > short, "源越长，输出预算应越大");
        // 无论多长，prompt + 输出都不该超过上下文窗口
        assert!(
            (3_000f64 * PROMPT_TOKENS_PER_CHAR) as u32 + long <= ctx,
            "prompt + 输出越界: {long}"
        );
        // 极短批也要给足最小生成空间
        assert!(output_token_budget(10, ctx, 0) >= 128);
    }

    /// 字符预算必须随上下文窗口缩放，且给输入 + 输出都留出空间。
    #[test]
    fn char_budget_scales_with_context() {
        let small = batch_char_budget(2048, 0);
        let large = batch_char_budget(8192, 0);
        assert!(large > small, "上下文越大，单批字符预算应越大");
        // 4096 上下文下的预算 ×（输入+输出系数）不得超过窗口
        let per = PROMPT_TOKENS_PER_CHAR + OUTPUT_TOKENS_PER_CHAR;
        assert!((small as f64 * per) <= 2048.0);
        assert!((large as f64 * per) <= 8192.0);
    }
    /// 离线链路前置检查：模型或程序缺失时必须报错（而非静默跑出 0 句的假成功）。
    #[test]
    fn ensure_ready_fails_when_model_or_cli_missing() {
        let engine = LLMEngine::new(
            "definitely-not-a-real-cli-path.exe",
            "definitely-not-a-real-model.gguf",
            4096,
            4,
        );
        let err = engine.ensure_ready().expect_err("缺模型/程序时应报错");
        let msg = err.to_string();
        assert!(
            msg.contains("模型未就位") || msg.contains("推理程序未就位"),
            "错误信息应说明缺什么: {msg}"
        );
    }

    // ─────────── P0-A：中文变体短路 ───────────

    /// 中文变体目标必须识别出来并映射到正确的 zhconv 变体；其它语言返回 None。
    #[test]
    fn chinese_variant_target_maps_variants() {
        assert_eq!(
            chinese_variant_target("繁体中文"),
            Some(zhconv::Variant::ZhHant)
        );
        assert_eq!(
            chinese_variant_target("简体中文"),
            Some(zhconv::Variant::ZhHans)
        );
        // 兼容语言码写法
        assert_eq!(
            chinese_variant_target("zh-Hant"),
            Some(zhconv::Variant::ZhHant)
        );
        assert_eq!(
            chinese_variant_target("zh-CN"),
            Some(zhconv::Variant::ZhHans)
        );
        // 非中文变体不得短路
        assert_eq!(chinese_variant_target("English"), None);
        assert_eq!(chinese_variant_target("日本語"), None);
        assert_eq!(chinese_variant_target("한국어"), None);
        assert_eq!(chinese_variant_target("Русский"), None);
        assert_eq!(chinese_variant_target("Français"), None);
        assert_eq!(chinese_variant_target("Deutsch"), None);
    }

    /// zhconv 的确定性转换：简→繁必须真的产出繁体字，且不等于原文。
    #[test]
    fn zhconv_simplified_to_traditional_changes_text() {
        let src = "大家好，下面我们一起来学习1.3，概率不等式。";
        let hant = zhconv::zhconv(src, zhconv::Variant::ZhHant);
        assert_ne!(hant, src, "简→繁应改变文本");
        assert!(
            hant.chars()
                .any(|c| matches!(c, '學' | '習' | '機' | '檢' | '講' | '個' | '會')),
            "应含繁体字: {hant}"
        );
        // 简→简 是恒等映射：这正是「简体中文目标 + 简体源」的例外，不能当失败
        assert_eq!(zhconv::zhconv(src, zhconv::Variant::ZhHans), src);
    }

    // ─────────── P0-B：译文 == 原文 复制检测 ───────────

    /// 真复制：长中文句一字不差吐回来 → 判为未译出。
    #[test]
    fn copy_detection_flags_long_chinese_copy() {
        let src = "那么这个概率不等式，其实整体里面考的相对来说不是很多。";
        assert!(is_untranslated_copy(src, src));
        // 只差空白字符也算复制
        assert!(is_untranslated_copy(
            src,
            "  那么这个概率不等式 ，其实整体里面考的相对来说不是很多。 "
        ));
    }

    /// 避免误杀：短句、纯数字、纯公式、纯拉丁术语「译文==原文」是**正确**的。
    #[test]
    fn copy_detection_spares_short_and_non_chinese() {
        // 短数字
        assert!(!is_untranslated_copy("1.3", "1.3"));
        // 短术语/符号
        assert!(!is_untranslated_copy("OK", "OK"));
        assert!(!is_untranslated_copy("P(A)", "P(A)"));
        // 长公式：不含 CJK，本就该原样保留
        assert!(!is_untranslated_copy(
            "P(A+B) = P(A) + P(B) - P(AB)",
            "P(A+B) = P(A) + P(B) - P(AB)"
        ));
        // 长英文术语/人名
        assert!(!is_untranslated_copy(
            "Bayesian inference",
            "Bayesian inference"
        ));
        // 正常译文（不同）当然不判复制
        assert!(!is_untranslated_copy(
            "那么这个概率不等式，其实整体里面考的相对来说不是很多。",
            "This probability inequality is not tested very much overall."
        ));
    }

    /// 复制率统计：只有样本足够多且过半才算「整篇摆烂」。
    #[test]
    fn copy_rate_alarm_threshold() {
        let low = CopyRate {
            rejected: 1,
            checked: 10,
        };
        assert!(!low.is_alarming());
        let high = CopyRate {
            rejected: 8,
            checked: 10,
        };
        assert!(high.is_alarming());
        assert!((high.rate() - 0.8).abs() < 1e-9);
        // 样本太少：比例没有统计意义
        let tiny = CopyRate {
            rejected: 2,
            checked: 3,
        };
        assert!(!tiny.is_alarming());
        // 空输入不应触发
        assert!(!CopyRate::default().is_alarming());
    }

    // ─────────── P0-C：固定开销参与上下文预算 ───────────

    /// 预算必须随固定开销（术语表）**单调下降**：术语表越长，单批能装的字幕越少。
    #[test]
    fn char_budget_shrinks_as_fixed_overhead_grows() {
        let ctx = 4096;
        let base = batch_char_budget(ctx, 0);
        let with_glossary = batch_char_budget(ctx, 8_000);
        assert!(
            with_glossary < base,
            "术语表开销必须让字符预算下降: {with_glossary} vs {base}"
        );
        // 单调性：开销越大预算越小
        let a = batch_char_budget(ctx, 1_000);
        let b = batch_char_budget(ctx, 2_000);
        assert!(a > b, "预算应随开销单调下降: {a} vs {b}");
    }

    /// 术语表很长时批大小收缩到仍能放进上下文。
    #[test]
    fn huge_glossary_shrinks_batches_to_fit_context() {
        let ctx = 4096;
        let glossary = "术语表（以下词条必须按给定译法翻译，不得改写）：".to_string()
            + &"甲=乙；".repeat(1_000);
        let fixed = fixed_prompt_tokens(&glossary);
        let budget = batch_char_budget(ctx, fixed);
        let per = PROMPT_TOKENS_PER_CHAR + OUTPUT_TOKENS_PER_CHAR;
        // 固定开销 + 一批输入 + 输出，不得越过上下文窗口
        assert!(
            fixed as f64 + budget as f64 * per <= ctx as f64,
            "固定开销 + 批次预算越界: fixed={fixed}, budget={budget}"
        );
        // 术语表已接近/超过上下文时，预算收缩到下限
        assert!(budget < batch_char_budget(ctx, 0));
    }

    /// 输出预算也要扣固定开销：术语表越长，留给输出的 token 越少。
    #[test]
    fn output_budget_accounts_for_fixed_overhead() {
        let ctx = 4096;
        let without = output_token_budget(1_000, ctx, 0);
        let with = output_token_budget(1_000, ctx, 2_000);
        assert!(
            with < without,
            "固定开销应压缩输出预算: {with} vs {without}"
        );
        assert!(with >= 128, "再紧也要给最小生成空间: {with}");
    }

    // ─────────── P1-D：llama-server 截断信号 ───────────

    /// 实测响应：`stop_type: "limit"` 表示撞上 `n_predict`；`"eos"` 表示正常收尾。
    #[test]
    fn llama_stop_type_detects_truncation() {
        let truncated = serde_json::json!({
            "content": "[1 译文] Good morning, let us learn",
            "stop": true,
            "truncated": false,
            "stop_type": "limit"
        });
        assert!(llama_stop_type_is_limit(&truncated));
        let normal = serde_json::json!({
            "content": "[1] Hello.\n[2] World.",
            "stop": true,
            "truncated": false,
            "stop_type": "eos"
        });
        assert!(!llama_stop_type_is_limit(&normal));
        // 字段缺失：宁可当成没截断，也不误报
        assert!(!llama_stop_type_is_limit(
            &serde_json::json!({ "content": "x" })
        ));
        assert!(!llama_stop_type_is_limit(&serde_json::json!({})));
    }

    /// 截断时丢弃**最后一条已解析译文**，让上层的折半重试把它补回来。
    #[test]
    fn truncated_batch_drops_last_parsed_line() {
        let expected = HashSet::from([1, 2, 3]);
        let raw = "[1] Hello.\n[2] World.\n[3] Wel";
        let mut parsed = LLMEngine::parse_batch_response(raw, &expected);
        assert_eq!(parsed.len(), 3);
        let last = [1usize, 2, 3]
            .iter()
            .rev()
            .copied()
            .find(|i| parsed.contains_key(i));
        assert_eq!(last, Some(3));
        parsed.remove(&3);
        assert_eq!(parsed.len(), 2, "末条应被丢弃以触发折半重试");
    }
}
