//! LLM 引擎 — 基于 llama.cpp 的字幕文本润色 (标点恢复、错别字修正、智能断句)

use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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

/// 单批至少要能装下的源字符数。
///
/// 低于它就是「固定开销把上下文吃光了」：再往下夹只会把**必然超窗**的请求伪装成
/// 「预算正常」——llama.cpp 会回 `EXCEED_CONTEXT_SIZE`，响应里没有 `content`，
/// 最终被上层笼统地报成「未能译出任何一句」。
const MIN_BATCH_CHARS: usize = 64;

/// 短句也至少给这么多输出 token，避免 `n_predict` 小到把译文掐断。
///
/// 只作用于 `want`（生成空间），**不**用来掩盖「固定开销已超窗」的 `remaining`。
const MIN_OUTPUT_TOKENS: u32 = 128;

/// 上下文里可用的 token 数（扣掉对话模板 / 特殊 token 的固定余量 [`CTX_MARGIN`]）。
fn usable_ctx_tokens(ctx_size: u32) -> u32 {
    ctx_size.saturating_sub(CTX_MARGIN)
}

/// 一批源文本至少需要的 token 数（输入 + 输出，按 [`MIN_BATCH_CHARS`] 折算）。
fn min_batch_tokens() -> u32 {
    ((MIN_BATCH_CHARS as f64) * (PROMPT_TOKENS_PER_CHAR + OUTPUT_TOKENS_PER_CHAR)).ceil() as u32
}

/// 固定开销（系统提示词模板 + 术语表 + 源语言线索 + 对话模板）是否已经**超过可用
/// 窗口**——即连一批最小输入都放不下。
///
/// 这是「术语表太长」的判定入口：判真时无论怎么缩小批次都装不下，调用方必须先裁剪
/// 术语表或直接报错，绝不能继续发请求——`batch_char_budget` 的触底只是症状，不是
/// 「预算正常」。
fn fixed_overhead_exceeds_window(ctx_size: u32, fixed_tokens: u32) -> bool {
    fixed_tokens.saturating_add(min_batch_tokens()) > usable_ctx_tokens(ctx_size)
}

/// 按上下文窗口推导「一批源文本的字符预算」。
///
/// 一批要同时装下「固定开销（系统提示词模板 + 术语表 + 源语言线索 + 对话模板）」
/// 「输入 prompt」与「模型输出」，后两者都随源字符数线性增长，因此
/// 预算 = (ctx - 固定开销 - 余量) / (输入系数 + 输出系数)。
///
/// `fixed_tokens` 必须包含**本次**系统提示词与术语表的实际长度：术语表理论上限
/// 是单条左右各 60 字符 × 80 条 ≈ 9.7k 字符（≈8k+ token），已是 4096 上下文的
/// 两倍，而且**每批重复注入**。
///
/// **返回 0 表示固定开销已超窗**（见 [`fixed_overhead_exceeds_window`]）：此前用
/// `.max(64)` 把它夹成「一批 64 字符」，等于把必然失败的请求伪装成正常预算；现在
/// 如实返回 0，调用方据此裁剪术语表 / 报错。
fn batch_char_budget(ctx_size: u32, fixed_tokens: u32) -> usize {
    if fixed_overhead_exceeds_window(ctx_size, fixed_tokens) {
        return 0;
    }
    let usable = usable_ctx_tokens(ctx_size).saturating_sub(fixed_tokens) as f64;
    let per_char = PROMPT_TOKENS_PER_CHAR + OUTPUT_TOKENS_PER_CHAR;
    (usable / per_char) as usize
}

/// 一次「术语表适配上下文」的判定结果（纯数据，供日志与单测断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlossaryFit {
    /// 本次实际注入的术语表文本（未超窗时与输入完全一致；超窗时是裁剪后的前缀）。
    pub text: String,
    /// 是否发生了裁剪。
    pub trimmed: bool,
    /// 裁剪前的词条数（按 [`GLOSSARY_ENTRY_SEP`] 切分；空表为 0）。
    pub before_entries: usize,
    /// 裁剪后的词条数。
    pub after_entries: usize,
    /// 裁剪前的字符数。
    pub before_chars: usize,
    /// 裁剪后的字符数。
    pub after_chars: usize,
}

/// 术语表词条分隔符（与 `utils::config` 的 `glossary_prompt()` 拼接保持一致）。
const GLOSSARY_ENTRY_SEP: char = '；';

/// 术语表提示里的词条数（表头与首条同属一段，故非空串 ≥1；空串为 0）。仅用于日志口径。
pub(crate) fn glossary_entry_count(glossary: &str) -> usize {
    if glossary.trim().is_empty() {
        0
    } else {
        glossary.split(GLOSSARY_ENTRY_SEP).count()
    }
}

/// 本次允许术语表占用的 token 上限（已扣掉 `reserved_tokens` 与一批最小输入输出）。
///
/// 返回 `None` 表示窗口里连最小批次都放不下——调用方须报「术语表过大 / `llm_ctx`
/// 太小」，而不是继续发请求。
fn glossary_token_budget(ctx_size: u32, reserved_tokens: u32) -> Option<u32> {
    usable_ctx_tokens(ctx_size)
        .checked_sub(reserved_tokens)?
        .checked_sub(min_batch_tokens())
}

/// 把术语表提示裁到 `max_tokens` 以内（**纯函数**：返回新串，绝不动用户配置）。
///
/// 优先级：表头与**靠前**的词条更重要——从尾部整条丢弃；若连第一条都放不下，退化
/// 为对文本做前缀字符截断（保住表头与首条的开头）。返回 `None` 表示 `max_tokens`
/// 小到放不下任何内容。
pub(crate) fn trim_glossary_to_tokens(glossary: &str, max_tokens: u32) -> Option<String> {
    if glossary.is_empty() {
        return Some(String::new());
    }
    if fixed_prompt_tokens(glossary) <= max_tokens {
        return Some(glossary.to_string());
    }
    if max_tokens == 0 {
        return None;
    }
    // ① 整条保留：表头 + 靠前的词条（累积到放不下为止）。
    let mut kept: Option<String> = None;
    for part in glossary.split(GLOSSARY_ENTRY_SEP) {
        let candidate = match &kept {
            None => part.to_string(),
            Some(prev) => format!("{prev}{GLOSSARY_ENTRY_SEP}{part}"),
        };
        if fixed_prompt_tokens(&candidate) > max_tokens {
            break;
        }
        kept = Some(candidate);
    }
    if let Some(kept) = kept {
        return Some(kept);
    }
    // ② 连第一条都放不下：前缀字符截断（保表头与首条开头）。
    let char_budget = (max_tokens as f64 / PROMPT_TOKENS_PER_CHAR).floor() as usize;
    let truncated: String = glossary.chars().take(char_budget).collect();
    if truncated.trim().is_empty() {
        None
    } else {
        Some(truncated)
    }
}

/// 判定「本次术语表」能否放进上下文；放不下就按优先级裁剪（**纯函数**）。
///
/// 返回 `None` 表示连最小批次都放不下（术语表再裁也没用）——调用方须报可读错误。
/// 返回的 [`GlossaryFit::text`] 只用于**本次提示词组装**，用户配置里的术语表原样
/// 保留（不做任何内存改写）。
pub(crate) fn fit_glossary_to_context(
    glossary: &str,
    ctx_size: u32,
    reserved_tokens: u32,
) -> Option<GlossaryFit> {
    let before_chars = glossary.chars().count();
    let before_entries = glossary_entry_count(glossary);
    let fixed_tokens = fixed_prompt_tokens(glossary) + reserved_tokens;
    if !fixed_overhead_exceeds_window(ctx_size, fixed_tokens) {
        return Some(GlossaryFit {
            text: glossary.to_string(),
            trimmed: false,
            before_entries,
            after_entries: before_entries,
            before_chars,
            after_chars: before_chars,
        });
    }
    let max_tokens = glossary_token_budget(ctx_size, reserved_tokens)?;
    let text = trim_glossary_to_tokens(glossary, max_tokens)?;
    Some(GlossaryFit {
        after_entries: glossary_entry_count(&text),
        after_chars: text.chars().count(),
        text,
        trimmed: true,
        before_entries,
        before_chars,
    })
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
///
/// 关键修复：不再对 `remaining` 也 `.max(128)`——那会把「固定开销已超窗」伪装成
/// 正常预算。真没空间时如实返回 0；`want` 上的下限只保证短句也有基本生成空间。
fn output_token_budget(source_chars: usize, ctx_size: u32, fixed_tokens: u32) -> u32 {
    let want = ((source_chars as f64 * OUTPUT_TOKENS_PER_CHAR) as u32).max(MIN_OUTPUT_TOKENS);
    let prompt_est = (source_chars as f64 * PROMPT_TOKENS_PER_CHAR) as u32 + CTX_MARGIN;
    let remaining = ctx_size
        .saturating_sub(prompt_est)
        .saturating_sub(fixed_tokens)
        .saturating_sub(64);
    want.min(remaining)
}

/// 把「系统提示词 + 术语表 + 源语言线索」的实际长度折算成 token 数。
///
/// 按 [`PROMPT_TOKENS_PER_CHAR`] 折算并向上取整：宁可高估（少装几条字幕），也不要
/// 低估（撑爆上下文，导致整批译文被截断且无法察觉）。
fn fixed_prompt_tokens(fixed_prompt: &str) -> u32 {
    (fixed_prompt.chars().count() as f64 * PROMPT_TOKENS_PER_CHAR).ceil() as u32
}

/// 上下文窗口的合法区间。
///
/// `llm_ctx` 来自 `config.toml`（用户可手改）。`0` 会让 llama.cpp 直接拒绝启动
/// （`-c 0` 连 KV cache 都建不出来），表现是「常驻服务起不来、命令行回退也失败」
/// 的连环失败，而且日志只有一句参数非法；极大值则会在启动时按窗口大小申请
/// KV cache，直接 OOM 崩掉。这里夹到可用区间：太小会让 [`batch_char_budget`]
/// 退化成「一批一句」，太大对字幕批量没有收益。
fn clamp_ctx_size(ctx_size: u32) -> u32 {
    const MIN_CTX: u32 = 512;
    const MAX_CTX: u32 = 65_536;
    let clamped = ctx_size.clamp(MIN_CTX, MAX_CTX);
    if clamped != ctx_size {
        warn!("llm_ctx={ctx_size} 超出可用区间 [{MIN_CTX}, {MAX_CTX}]，已夹为 {clamped}");
    }
    clamped
}

/// LLM 推理线程数的有效区间。
///
/// 下界 1（0 会让 llama.cpp 拒绝启动）；上界 64：`llm_threads` 是用户可手改的全局
/// 共享项，只夹下界时一个离谱的大值会原样透传给 llama.cpp（`-t` / 常驻服务），
/// 线程远超物理核数只会增加同步开销、反而拖慢解码。
const MIN_LLM_THREADS: u32 = 1;
const MAX_LLM_THREADS: u32 = 64;

/// 把线程数夹到 [`MIN_LLM_THREADS`, `MAX_LLM_THREADS`]。
///
/// 与同级引擎（`punc.rs` / `sensevoice.rs` 的 `clamp_threads`）保持一致：两边都得
/// 夹，不能只有下界。
fn clamp_threads(threads: u32) -> u32 {
    threads.clamp(MIN_LLM_THREADS, MAX_LLM_THREADS)
}

/// 命令行回退路径（[`LLMEngine::run_prompt`]）的等待上限。
///
/// 命令行路径**没有**常驻服务那种读超时：`Command::output()` 一旦卡住就是永久
/// 阻塞，用户在界面上点「终止」也杀不掉它（取消标志只在批次之间被检查）。因此按
/// 「本批最多生成多少 token」给出等待上限，超时/取消都终止并回收子进程，把
/// 「整条流水线卡死」降级成「这一批失败，其余批次继续」。
///
/// 0.5B 模型 CPU 推理实测每 token 几十毫秒，这里按 0.5s/token 给足余量；下限 60s
/// （模型首次加载本身就要几秒到十几秒），上限 15 分钟。
fn cli_wait_budget(max_tokens: u32) -> Duration {
    const MIN_SECS: u64 = 60;
    const MAX_SECS: u64 = 900;
    const PER_TOKEN_MS: u64 = 500;
    let want_secs = (max_tokens as u64).saturating_mul(PER_TOKEN_MS) / 1000;
    Duration::from_secs(want_secs.clamp(MIN_SECS, MAX_SECS))
}

/// 起一个后台线程把子进程管道（stdout / stderr）抽干，返回该线程的句柄。
///
/// 管道缓冲区只有几十 KB：没人读的话，子进程写满就阻塞在 write 上，而父进程又在
/// 等它退出——双方一起卡死。`None` 表示这条管道没有被 pipe（无可读）。
fn drain_pipe<P: Read + Send + 'static>(
    pipe: Option<P>,
) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    pipe.map(|mut p| {
        std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            let _ = p.read_to_end(&mut buf);
            buf
        })
    })
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
            "检出 {}/{} 句未真正译出（模型疑似复制原文），可再次点击「开始翻译」补译",
            self.rejected, self.checked
        )
    }
}

/// 复制率计数：候选句里**至今仍未译出**的那些才计入 `rejected`。
///
/// 抽成纯函数便于单测——`checked`（分母）由调用方累加。
fn count_untranslated_copies(
    segments: &[Segment],
    candidates: &[usize],
    target_lang: &str,
) -> usize {
    candidates
        .iter()
        .filter(|&&pos| !segments[pos].translation_matches(target_lang))
        .count()
}

/// 折半重试对单条候选译文的判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryVerdict {
    /// 通过 [`validate_translation`]，可以写回。
    Accept,
    /// 近似复制（`Echo`）：保持未译，并计入复制率。
    RejectCopy,
    /// 其他不合格（改动过大 / 空输出）：保持未译，不计入复制率。
    RejectOther,
}

/// 折半重试写回前的判定（**纯函数**）。
///
/// 此前这里只判「完全等于原文」（[`is_untranslated_copy`]），于是**近似复制**
/// （改一个功能字 / 加个标点 / 扩写）会被静默写进用户字幕，还不计入复制率、
/// 绕过 `is_alarming` 兜底。现在与主路径**共用同一条** [`validate_translation`]：
/// 只有 `Ok` 才写回。
pub(crate) fn classify_retry_outcome(source: &str, translation: &str) -> RetryVerdict {
    match validate_translation(source, translation) {
        RewriteVerdict::Ok => RetryVerdict::Accept,
        RewriteVerdict::Echo => RetryVerdict::RejectCopy,
        RewriteVerdict::Empty | RewriteVerdict::SuspectTooDifferent => RetryVerdict::RejectOther,
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

// ─────────── LLM 输出校验自愈闭环（相似度阈值 + 一次反思重试 + 回退） ───────────
//
// 既有能力只有「形式校验」：译文 == 原文拒收、截断检测、序号结构校验。缺三样：
// ① 逐句相似度校验；② 低置信回退；③ 反思重译。这里补齐，且全部在引擎内完成：
// 不新增第三方依赖、不联网、不动上层 UI。

/// 相似度比对的最大字符数：超长文本截断到前 N 个字符再比。
///
/// 字幕单条通常几十字；超过这个长度的多是模型跑飞 / 复读。截断把哈希计数的常数
/// 压住，而且对「是不是同一句」的判定没有影响（前 N 个实义字符已足够代表全句）。
const SIMILARITY_MAX_CHARS: usize = 4_096;

/// 相似度 ≥ 该值判为 [`RewriteVerdict::Echo`]——模型几乎没有改动，等于「没干活」。
///
/// 归一化已抹掉空白与标点，所以「只补标点」的润色相似度就是 1.0；真正动了词的润色
/// 会掉到 0.9 以下。0.97 是「只差一两个功能字」的分界；再往高取会把「顺手改了个
/// 错别字」的正常润色误判成没干活。
const ECHO_SIMILARITY_THRESHOLD: f64 = 0.97;

/// 相似度 < 该值判为 [`RewriteVerdict::SuspectTooDifferent`]——模型把句子重写了。
///
/// 同语言润色的正常相似度落在 0.7~1.0（补标点 = 1.0、改错别字 ≈ 0.85、顺句 ≈ 0.7）。
/// 低于约 1/3 说明保留下来的实义字符不足三分之一，基本是另起一句，必须拦下。
/// 取值刻意偏低（0.35），遵循「宁可放过、不可错杀」：错杀会把正常润色回退成原文，
/// 而放过只是把可疑句再问模型一次，代价小得多。
const SUSPECT_SIMILARITY_THRESHOLD: f64 = 0.35;

/// 翻译「改动过大」的**长度膨胀上限**（译文归一化字符数 / 原文归一化字符数）。
///
/// CJK→拉丁方向字符数天然膨胀（一个汉字常对应数个拉丁字母），实测 3~5 倍都算正常；
/// 取 8 倍只拦「模型把一句话扩写成一整段」的失控输出。
const TRANSLATION_MAX_EXPANSION: f64 = 8.0;

/// 翻译「改动过大」的**长度萎缩下限**。拉丁→CJK 会大幅缩短，实测可到 1/4~1/5；
/// 取 1/12 只拦「模型把长句压成两三个字」的丢内容输出。
const TRANSLATION_MIN_EXPANSION: f64 = 1.0 / 12.0;

/// 触发长度失控判定的最小原文长度（归一化字符）：太短的句子长度比没有统计意义。
const TRANSLATION_LENGTH_MIN_CHARS: usize = 8;

/// 反思重试的次数上限（显式常量，避免烧钱 / 卡住）。**1** = 对可疑句只重问一次；
/// 再不合格直接回退，不再纠缠。
const REFLECT_MAX_ATTEMPTS: usize = 1;

/// 反思重试的提示词（外置常量，便于以后统一搬迁到提示词目录）。
///
/// 为什么这么写：可疑句的病因是「模型擅自改写 / 扩写 / 丢内容」，所以首要指令是
/// **约束改动幅度**（与原句严格一一对应、长度相当、语义不变、只做最小必要修改），
/// 其次明确「本就无需修改就原样返回」——否则小模型会为了「有改动」而乱改，制造新的
/// 可疑句。翻译场景按目标语言如实翻译，润色场景只补标点 / 纠正明显错别字。
pub(crate) const REFLECT_PROMPT: &str = "你上一版对下面每条字幕的改动过大（明显偏离对应原句），已判定不可用。请重做：每条都必须与原句严格一一对应、长度相当、语义不变，只做最小必要的修改——翻译场景按目标语言如实翻译，润色场景只补标点并纠正明显错别字；不得重写整句、不得扩写、不得增删意思、不得合并或拆分条目。若某条本就无需修改，原样返回即可。";

/// 一次输出校验的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RewriteVerdict {
    /// 改动幅度正常，可以采纳。
    Ok,
    /// 与原句差别过大（疑似重写 / 扩写 / 丢内容），需要反思重试或回退。
    SuspectTooDifferent,
    /// 输出为空或纯空白。
    Empty,
    /// 与原句几乎完全相同——模型没干活（翻译场景则是复制原文）。
    Echo,
}

/// 校验自愈的统计，供上层提示与日志；字段名即上层可直接读取的口径。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RewriteStats {
    /// 参与校验的模型输出条数（分母）。
    pub checked: usize,
    /// 被判定「改动过大」且反思后仍不合格、已回退的条数。
    pub fallback: usize,
    /// 疑似「没干活」（与原句几乎相同）的条数。
    pub echo: usize,
    /// 空输出条数。
    pub empty: usize,
    /// 实际发起的反思重试次数（上限 [`REFLECT_MAX_ATTEMPTS`]）。
    pub reflect_calls: usize,
}

impl RewriteStats {
    /// 汇总另一批统计（分批循环逐批累加）。
    fn merge(&mut self, other: RewriteStats) {
        self.checked += other.checked;
        self.fallback += other.fallback;
        self.echo += other.echo;
        self.empty += other.empty;
        self.reflect_calls += other.reflect_calls;
    }

    /// 是否发生过自愈动作（有回退或发生过反思重试）。
    pub(crate) fn has_selfheal(&self) -> bool {
        self.fallback > 0 || self.reflect_calls > 0
    }

    /// 给用户的提示文案（同时用于日志与进度回调）。
    pub(crate) fn warning_message(&self) -> String {
        format!(
            "检出 {} 句模型输出改动过大，已回退为原文 / 保持未译（反思重试 {} 次），可再次点击重试补做",
            self.fallback, self.reflect_calls
        )
    }
}

/// 相似度用的归一化：Unicode 小写 + 丢弃所有非字母数字字符（空白、标点、符号）。
///
/// `char::is_alphanumeric` 对 CJK 表意字返回 true，因此汉字会被保留；中英混排时
/// 拉丁字母与汉字一起参与计数，跨语种句子的相似度就是「共享字符占比」。
fn similarity_chars(s: &str) -> Vec<char> {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_alphanumeric())
        .take(SIMILARITY_MAX_CHARS)
        .collect()
}

/// 两段文本的字符**多重集**相似度（Dice 系数），取值 0.0~1.0。
///
/// 为什么用「字符多重集 Dice」而不是 LCS / 编辑距离：
/// - 我们只判「是不是同一句话内容漂移了」，不需要逐位对齐；Dice 对语序调整、标点
///   增删不敏感，正合这个用途；
/// - Dice 用哈希计数实现是 O(n)，LCS 是 O(n·m)——字幕批量里 n、m 可达数百，
///   差值不划算，也没有必要；
/// - 只依赖标准库 `HashMap`，不引入任何新依赖。
///
/// 边界：两边归一化后都为空 → 1.0（都「没有实义内容」，视为一致）；仅一边为空 → 0.0。
pub(crate) fn similarity(a: &str, b: &str) -> f64 {
    let a = similarity_chars(a);
    let b = similarity_chars(b);
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut counts: std::collections::HashMap<char, i64> =
        std::collections::HashMap::with_capacity(a.len());
    for c in &a {
        *counts.entry(*c).or_insert(0) += 1;
    }
    let mut overlap = 0i64;
    for c in &b {
        if let Some(n) = counts.get_mut(c) {
            if *n > 0 {
                *n -= 1;
                overlap += 1;
            }
        }
    }
    2.0 * overlap as f64 / (a.len() + b.len()) as f64
}

/// 校验一条**同语言改写**输出（润色 / 纠错 / 智能断句）。
///
/// 判据见三个阈值常量：[`ECHO_SIMILARITY_THRESHOLD`] 与
/// [`SUSPECT_SIMILARITY_THRESHOLD`]。空输出直接判 [`RewriteVerdict::Empty`]；
/// 原文本身没有实义内容时无从比较，按 [`RewriteVerdict::Ok`] 放行。
pub(crate) fn validate_rewrite(original: &str, rewritten: &str) -> RewriteVerdict {
    if rewritten.trim().is_empty() {
        return RewriteVerdict::Empty;
    }
    if similarity_chars(original).is_empty() {
        return RewriteVerdict::Ok;
    }
    let sim = similarity(original, rewritten);
    if sim >= ECHO_SIMILARITY_THRESHOLD {
        return RewriteVerdict::Echo;
    }
    if sim < SUSPECT_SIMILARITY_THRESHOLD {
        return RewriteVerdict::SuspectTooDifferent;
    }
    RewriteVerdict::Ok
}

/// 校验一条**跨语言译文**。
///
/// 与 [`validate_rewrite`] 的关键区别：译文与原文本就不同语言，字符相似度天然很低，
/// 不能拿「相似度下限」当可疑判据（否则每一句都会被误杀）。翻译场景的「改动过大」
/// 表现为**长度失控**（一句扩写成一段 / 长句压成两个字），复制原文则表现为相似度
/// 接近 1。三段判定与 [`is_untranslated_copy`] 的守卫同口径（短句、纯拉丁放行），
/// 保证「OK→OK」「1.3→1.3」这类本就该原样保留的句子不被误伤。
pub(crate) fn validate_translation(source: &str, translation: &str) -> RewriteVerdict {
    if translation.trim().is_empty() {
        return RewriteVerdict::Empty;
    }
    // ① 一字不差的复制（原判据）。
    if is_untranslated_copy(source, translation) {
        return RewriteVerdict::Echo;
    }
    let src = similarity_chars(source);
    if src.is_empty() {
        return RewriteVerdict::Ok;
    }
    // ② 含 CJK 的长句被**近乎原样**吐回（只改了个别字）——同判为复制。
    if source.chars().count() >= 4
        && source.chars().any(is_cjk)
        && similarity(source, translation) >= ECHO_SIMILARITY_THRESHOLD
    {
        return RewriteVerdict::Echo;
    }
    // ③ 长度失控（扩写 / 丢内容）。仅对足够长的句子判，短句比值没有意义。
    if src.len() >= TRANSLATION_LENGTH_MIN_CHARS {
        let dst = similarity_chars(translation).len();
        let ratio = dst as f64 / src.len() as f64;
        if ratio > TRANSLATION_MAX_EXPANSION || ratio < TRANSLATION_MIN_EXPANSION {
            return RewriteVerdict::SuspectTooDifferent;
        }
    }
    RewriteVerdict::Ok
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
        let mut child = cmd
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
            // 子进程已经自行退出（端口被别人抢了 / 模型坏了 / 参数被拒）：它不可能
            // 再就绪；更不能把「恰好占着这个端口的别人」当成自己的服务——那会让
            // 之后每一批请求都发给陌生进程，白白多走一轮失败 + 命令行回退。
            if matches!(child.try_wait(), Ok(Some(_))) {
                warn!("llama-server 启动后立即退出，放弃常驻服务，回退命令行");
                let _ = child.wait();
                return None;
            }
            if TcpStream::connect_timeout(&address.parse().ok()?, Duration::from_millis(250))
                .is_ok()
            {
                return Some(Self { child, address });
            }
            std::thread::sleep(Duration::from_millis(100));
        }

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
        // 六处失败路径此前都是裸 `.ok()?`：互相不可区分、函数体内零日志，于是
        // 「术语表撑爆上下文 → 缺 content」被上层笼统地伪装成「模型没装好」。这里
        // 逐条加**诊断日志**（只记录、不把失败改成功），便于定位真正的病因。
        let mut stream = match self.address.parse::<std::net::SocketAddr>() {
            Ok(address) => match TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
                Ok(stream) => stream,
                Err(e) => {
                    debug!(error = %e, "连接 llama-server 失败，回退命令行");
                    return None;
                }
            },
            Err(e) => {
                debug!(error = %e, address = %self.address, "llama-server 地址解析失败，回退命令行");
                return None;
            }
        };
        if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(120))) {
            debug!(error = %e, "设置 llama-server 读超时失败，回退命令行");
            return None;
        }
        if let Err(e) = stream.set_write_timeout(Some(Duration::from_secs(5))) {
            debug!(error = %e, "设置 llama-server 写超时失败，回退命令行");
            return None;
        }
        let request = format!(
            "POST /completion HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.address,
            body.len(),
            body
        );
        if let Err(e) = stream.write_all(request.as_bytes()) {
            debug!(error = %e, "发送 llama-server /completion 请求失败，回退命令行");
            return None;
        }
        let mut response = String::new();
        if let Err(e) = stream.read_to_string(&mut response) {
            debug!(error = %e, "读取 llama-server 响应失败（可能是读超时），回退命令行");
            return None;
        }
        let Some((_, payload)) = response.split_once("\r\n\r\n") else {
            debug!("llama-server 响应缺少 HTTP 头分隔，回退命令行");
            return None;
        };
        let value = match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(value) => value,
            Err(e) => {
                debug!(
                    error = %e,
                    head = %response_head(payload, 200),
                    "llama-server 响应不是合法 JSON，回退命令行"
                );
                return None;
            }
        };
        let Some(content) = value.get("content").and_then(|c| c.as_str()) else {
            // 缺 `content` 正是 llama.cpp 在 prompt 超过 `-c` 时的响应形态
            // （`EXCEED_CONTEXT_SIZE`）：此前静默返回 None，失败被伪装成「模型没装好」。
            debug!(
                head = %response_head(payload, 200),
                "llama-server 响应缺少 content 字段（常见于 prompt 超过 -c / EXCEED_CONTEXT_SIZE），回退命令行"
            );
            return None;
        };
        Some(CompletionOutcome {
            content: content.to_owned(),
            // P1-D：llama.cpp 的 `/completion` 在撞上 `n_predict` 时会给出
            // `stop_type: "limit"`（实测），这是「因长度截断」的可靠信号。
            // `truncated` 是「prompt 被截断」的旧字段，语义不同，不能拿来当
            // 输出截断判据（实测正常响应里它恒为 false）。
            stopped_by_limit: llama_stop_type_is_limit(&value),
        })
    }
}

/// 日志里截取响应正文开头（避免把整段响应写进日志）。
fn response_head(payload: &str, max: usize) -> String {
    payload.chars().take(max).collect()
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

/// 命令行回退路径的截断判据：从 llama.cpp 的计时统计里取「本次生成阶段生成的 token 数」。
///
/// 命令行路径拿不到 `/completion` 的 `stop_type`，但 llama.cpp 会把性能统计打到 stderr，
/// 其中生成阶段那一行形如：
/// `llama_perf_context_print:        eval time =   1234.56 ms /   512 runs   ( ... )`
/// 结尾的 `N runs` 就是本次生成的 token 数；`N >= max_tokens` 说明是被 `-n` 顶停的。
/// 解析不到（不同版本日志格式、被 --no-perf 关掉）返回 false——宁可漏报也不能把
/// 完整输出误判成截断（误判会白白丢掉一条完整译文）。
fn cli_stderr_reports_limit(stderr: &str, max_tokens: u32) -> bool {
    if max_tokens == 0 {
        return false;
    }
    let Ok(re_runs) = regex::Regex::new(r"/\s*(\d+)\s+runs") else {
        return false;
    };
    stderr.lines().any(|line| {
        // 只认生成阶段那一行：`prompt eval time` 行的 `N runs` 是**输入** token 数，
        // 通常远大于 max_tokens，若不排除会把每次请求都误判成截断。
        if !line.contains("eval time") || line.contains("prompt") {
            return false;
        }
        re_runs
            .captures(line)
            .and_then(|caps| caps.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .map(|runs| runs >= max_tokens as u64)
            .unwrap_or(false)
    })
}

/// 输出被长度上限截断时丢弃「最后一条已解析结果」，返回被丢弃的序号。
/// `order` 是本批的序号顺序（按批内顺序），用于确定「最后一条」。
fn drop_truncated_tail(
    result: &mut std::collections::HashMap<usize, String>,
    order: &[usize],
    stopped_by_limit: bool,
) -> Option<usize> {
    if !stopped_by_limit {
        return None;
    }
    let last_matched = order
        .iter()
        .rev()
        .copied()
        .find(|index| result.contains_key(index))?;
    result.remove(&last_matched);
    Some(last_matched)
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
    /// 最近一次润色 / 翻译的「输出校验自愈」统计（供上层提示）。
    /// 与 `glossary` 同一套理由用 `Mutex` 持有：引擎挂在 `Arc` 后面共享、
    /// 写回时拿不到 `&mut self`；每批写一次，开销可忽略。
    rewrite_stats: Mutex<RewriteStats>,
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
            ctx_size: clamp_ctx_size(ctx_size),
            threads: std::sync::atomic::AtomicU32::new(clamp_threads(threads)),
            cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            glossary: Mutex::new(String::new()),
            rewrite_stats: Mutex::new(RewriteStats::default()),
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
            .store(clamp_threads(threads), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn threads(&self) -> u32 {
        self.threads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 最近一次润色 / 翻译中「输出校验自愈」的统计（供上层提示用户）。
    pub fn last_rewrite_stats(&self) -> RewriteStats {
        *self.rewrite_stats.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 写回本次任务的校验自愈统计（任务起手与收尾各调用一次）。
    fn store_rewrite_stats(&self, stats: RewriteStats) {
        *self.rewrite_stats.lock().unwrap_or_else(|e| e.into_inner()) = stats;
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
            .map(|outcome| Self::clean_response(&outcome.content, text))
            .unwrap_or_else(|| text.to_string())
    }

    fn run_prompt(&self, prompt: &str, max_tokens: u32) -> Option<CompletionOutcome> {
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
        let mut child = cli_cmd
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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;

        // 必须**边跑边读** stdout：管道缓冲区只有几 KB，长批的译文（上千 token）
        // 一旦把它写满，子进程会阻塞在 write 上，而父进程又在等它退出——双方一起
        // 卡死。读线程负责抽干 stdout，父线程只负责等待与超时。
        // stderr 一起抽干：既避免同样的写满互等，也保住失败时的排障信息。
        let stdout_reader = drain_pipe(child.stdout.take());
        let stderr_reader = drain_pipe(child.stderr.take());

        // 有界等待：正常退出、取消、超时三种出口都走这里，出口之后一律 kill + wait
        // 回收，绝不留下挂着不退的子进程。
        //
        // 没有直接复用 `engines::ffmpeg::run_with_deadline`：那套执行器只有截止期、
        // 没有取消钩子，而这条路径必须能立刻响应界面的「终止」（长批可能跑几分钟）。
        let budget = cli_wait_budget(max_tokens);
        let deadline = Instant::now() + budget;
        let mut status = None;
        loop {
            match child.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, "等待 llama.cpp 润色进程失败");
                    break;
                }
            }
            if self.is_cancelled() {
                warn!("润色/翻译被取消，终止命令行 llama.cpp 子进程");
                break;
            }
            if Instant::now() >= deadline {
                warn!(
                    seconds = budget.as_secs(),
                    "命令行 llama.cpp 超过等待上限仍未退出，终止该批（其余批次继续）"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if status.is_none() {
            let _ = child.kill();
        }
        // kill 之后必须 wait：只 kill 不 wait 会留下未回收的进程条目。
        let _ = child.wait();
        let stdout_bytes = stdout_reader
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        // 进程已退出，join 安全。stderr 不再只在失败分支取：成功分支也要拿它解析
        // 生成阶段的 token 数，才能判断输出是否被 `-n` 顶停（见
        // [`cli_stderr_reports_limit`]）。
        let stderr_bytes = stderr_reader
            .and_then(|h| h.join().ok())
            .unwrap_or_default();

        // 进程已退出，提示词文件不再需要：立刻删除（守卫仍会在返回时兜底）
        prompt_guard.remove_now();

        match status {
            Some(s) if s.success() => Some(CompletionOutcome {
                content: String::from_utf8_lossy(&stdout_bytes).into_owned(),
                stopped_by_limit: cli_stderr_reports_limit(
                    &String::from_utf8_lossy(&stderr_bytes),
                    max_tokens,
                ),
            }),
            Some(s) => {
                let err = String::from_utf8_lossy(&stderr_bytes);
                let tail: String = err
                    .chars()
                    .rev()
                    .take(400)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                warn!(
                    "llama.cpp 润色退出失败: {:?}；stderr 尾部: {}",
                    s.code(),
                    tail.trim()
                );
                None
            }
            // 取消 / 超时：日志已在上面记过，这里保持「本批无结果」。
            None => None,
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
        self.store_rewrite_stats(RewriteStats::default());
        info!("LLM 开始润色 {} 个字幕片段", total);
        if let Some(ref cb) = progress_cb {
            cb(0.0, "正在加载 Qwen 模型，首次启动通常需要几秒...");
        }
        let server = self.start_server();
        if server.is_some() {
            info!("Qwen 常驻服务已就绪，将复用已加载模型完成批量润色");
        } else {
            warn!("Qwen 常驻服务不可用，回退到命令行批量润色");
        }

        // 取数与写回分离：批次循环收敛到 `polish_apply_selfheal`，取数与反思回调都可
        // 替换。生产链路走 `polish_batch` / `polish_reflect`（常驻服务失败自动回退
        // 命令行）；回归测试注入构造结果，复现「第二遍只返回部分段落」的场景。
        //
        // 常驻服务用 `RefCell` 包一层：取数回调与反思回调都要借用同一个
        // `Option<LlamaServer>`，裸 `&mut` 会被借用检查器拒绝；两者在运行时是**顺序**
        // 借用（先取数、后反思），不会同时持有。
        let server_cell = std::cell::RefCell::new(server);
        let stats = self.polish_apply_selfheal(
            &mut segments,
            progress_cb,
            |batch| self.polish_batch(batch, server_cell.borrow_mut().as_mut()),
            |suspects| self.polish_reflect(suspects, server_cell.borrow_mut().as_mut()),
        );
        self.store_rewrite_stats(stats);
        if stats.has_selfheal() {
            warn!(
                fallback = stats.fallback,
                echo = stats.echo,
                reflect_calls = stats.reflect_calls,
                "润色输出校验自愈：{} 句改动过大已回退（含最多一次反思重试）",
                stats.fallback
            );
        }

        info!("LLM 全部片段润色完成");
        Ok(segments)
    }

    /// 润色批次循环的**兼容入口**：切批取数并写回 `segments`。
    ///
    /// 保留原签名是为了让既有回归测试不必改写；内部直接走
    /// [`Self::polish_apply_selfheal`]，但**不注入反思回调**——此时可疑句一律按
    /// 回退处理（与「拿不到结果就保留原文」同口径）。
    #[cfg(test)]
    fn polish_apply<F>(
        &self,
        segments: &mut [Segment],
        progress_cb: Option<crate::engines::TextProgressCb>,
        fetch: F,
    ) where
        F: FnMut(&[Segment]) -> std::collections::HashMap<usize, String>,
    {
        let _ = self.polish_apply_selfheal(segments, progress_cb, fetch, |_| {
            std::collections::HashMap::new()
        });
    }

    /// 润色批次循环（含**输出校验自愈**）：切批取数、逐句校验、写回。
    ///
    /// `fetch` 取一批润色结果，`reflect` 对「改动过大」的可疑句做**最多一次**反思
    /// 重试。两者都由调用方注入，测试可在不启动模型的前提下驱动**与生产完全一致**
    /// 的切批、校验与写回逻辑（这也正是「反思只重试一次」能被断言的原因）。
    fn polish_apply_selfheal<F, R>(
        &self,
        segments: &mut [Segment],
        progress_cb: Option<crate::engines::TextProgressCb>,
        mut fetch: F,
        mut reflect: R,
    ) -> RewriteStats
    where
        F: FnMut(&[Segment]) -> std::collections::HashMap<usize, String>,
        R: FnMut(&[(usize, String, String)]) -> std::collections::HashMap<usize, String>,
    {
        let total = segments.len();
        let mut completed = 0usize;
        let mut stats = RewriteStats::default();
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
                let results = fetch(&group[start..end]);
                // 输出校验：逐句判 Ok / SuspectTooDifferent / Empty / Echo。
                // 「改动过大」的可疑句先做一次反思重试，仍不合格才回退——回退口径是
                // 「accepted 里没有该序号」，`apply_polish_results` 会保留上一遍润色 /
                // 原文，绝不会把可疑内容静默写进字幕。
                let originals: std::collections::HashMap<usize, String> = group[start..end]
                    .iter()
                    .map(|seg| (seg.index, seg.text.clone()))
                    .collect();
                let (accepted, batch_stats) =
                    Self::selfheal_batch(validate_rewrite, true, &originals, results, &mut reflect);
                stats.merge(batch_stats);
                Self::apply_polish_results(&mut group[start..end], &accepted);
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
        // 发生过自愈就通过进度回调提示上层——「不允许把可疑内容静默写进用户字幕」。
        if stats.has_selfheal() {
            if let Some(ref cb) = progress_cb {
                cb(1.0, &stats.warning_message());
            }
        }
        stats
    }

    /// 一批「已解析的模型输出」的校验自愈核心（纯逻辑，不做任何模型调用）。
    ///
    /// 流程：逐条校验 → 收集「改动过大」的可疑句 → **最多一次**反思重试（`reflect`
    /// 回调由调用方注入真实模型，测试注入假回调）→ 重试结果再校验 → 仍不合格的回退。
    ///
    /// 回退口径由调用方决定（本函数只把不合格项从 `accepted` 中剔除）：
    /// - 润色（`echo_is_accept = true`）：不采纳 → 保留上一遍润色 / 原文；Echo 视为
    ///   正常结果采纳（「只补标点」本就是合格润色，不能算失败）；
    /// - 翻译（`echo_is_accept = false`）：不采纳 → 保持未译；Echo 视为「没译出」剔除。
    ///
    /// 反思循环写成 `for _ in 0..REFLECT_MAX_ATTEMPTS` 而非「只写一遍」，是为了让
    /// 「最多几次」由常量决定且可被测试断言（`reflect_calls`）。
    fn selfheal_batch<G>(
        verdict_of: fn(&str, &str) -> RewriteVerdict,
        echo_is_accept: bool,
        originals: &std::collections::HashMap<usize, String>,
        first: std::collections::HashMap<usize, String>,
        mut reflect: G,
    ) -> (std::collections::HashMap<usize, String>, RewriteStats)
    where
        G: FnMut(&[(usize, String, String)]) -> std::collections::HashMap<usize, String>,
    {
        let mut stats = RewriteStats::default();
        let mut accepted: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        let mut suspects: Vec<(usize, String, String)> = Vec::new();

        for (index, text) in first {
            let Some(original) = originals.get(&index) else {
                continue;
            };
            let original = original.clone();
            stats.checked += 1;
            match verdict_of(&original, &text) {
                RewriteVerdict::Ok => {
                    accepted.insert(index, text);
                }
                RewriteVerdict::Echo => {
                    stats.echo += 1;
                    if echo_is_accept {
                        accepted.insert(index, text);
                    }
                }
                RewriteVerdict::Empty => stats.empty += 1,
                RewriteVerdict::SuspectTooDifferent => {
                    suspects.push((index, original, text));
                }
            }
        }

        if suspects.is_empty() {
            return (accepted, stats);
        }

        let mut remaining = suspects;
        for _ in 0..REFLECT_MAX_ATTEMPTS {
            if remaining.is_empty() {
                break;
            }
            stats.reflect_calls += 1;
            let healed = reflect(&remaining);
            let mut still: Vec<(usize, String, String)> = Vec::new();
            for (index, original, _) in remaining {
                let ok = match healed.get(&index) {
                    Some(text) => match verdict_of(&original, text) {
                        RewriteVerdict::Ok => {
                            accepted.insert(index, text.clone());
                            true
                        }
                        RewriteVerdict::Echo if echo_is_accept => {
                            accepted.insert(index, text.clone());
                            true
                        }
                        _ => false,
                    },
                    None => false,
                };
                if !ok {
                    still.push((index, original, String::new()));
                }
            }
            remaining = still;
        }
        // 反思后仍不合格：回退（不写回），计入统计。
        stats.fallback += remaining.len();
        (accepted, stats)
    }

    /// 组装反思重试提示词：系统段 = [`REFLECT_PROMPT`]（+ 场景补充），
    /// 用户段**只列可疑句**（原句 + 被拒的上一版），避免让模型重复处理没问题的句子。
    fn build_reflect_prompt(suspects: &[(usize, String, String)], system_extra: &str) -> String {
        let mut user = String::new();
        for (index, original, previous) in suspects {
            user.push_str(&format!(
                "[{index}] 原句：{original}\n上一版（改动过大，已弃用）：{previous}\n"
            ));
        }
        format!(
            "<|im_start|>system\n{REFLECT_PROMPT}{system_extra}必须逐行输出，格式严格为「[序号] 文本」，不得解释、不得合并、不得遗漏、不得改动序号。<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n"
        )
    }

    /// 对一批「改动过大」的可疑句做一次反思重试（润色链路）。
    ///
    /// 拿到结果后由 [`Self::selfheal_batch`] 再校验一遍；这里只负责「问一次」。
    fn polish_reflect(
        &self,
        suspects: &[(usize, String, String)],
        server: Option<&mut LlamaServer>,
    ) -> std::collections::HashMap<usize, String> {
        let prompt = Self::build_reflect_prompt(suspects, "");
        let source_chars: usize = suspects
            .iter()
            .map(|(_, orig, _)| orig.chars().count())
            .sum();
        let max_tokens =
            output_token_budget(source_chars, self.ctx_size, POLISH_FIXED_PROMPT_TOKENS);
        self.run_reflect_prompt(&prompt, max_tokens, server, suspects)
    }

    /// 对一批「改动过大」的可疑句做一次反思重试（翻译链路）。
    fn translate_reflect(
        &self,
        suspects: &[(usize, String, String)],
        target_lang: &str,
        glossary: &str,
        server: Option<&mut LlamaServer>,
    ) -> std::collections::HashMap<usize, String> {
        let extra = format!(
            "（目标语言：{target_lang}；{glossary}只翻译为{target_lang}，保持与原文长度相当。）"
        );
        let prompt = Self::build_reflect_prompt(suspects, &extra);
        let source_chars: usize = suspects
            .iter()
            .map(|(_, orig, _)| orig.chars().count())
            .sum();
        let max_tokens = output_token_budget(source_chars, self.ctx_size, PROMPT_HINT_TOKENS);
        self.run_reflect_prompt(&prompt, max_tokens, server, suspects)
    }

    /// 反思提示词的**统一调用 + 解析**：常驻服务失败自动回退命令行，解析复用
    /// [`Self::parse_batch_response`]，并沿用与批量路径一致的「截断丢末条」策略。
    fn run_reflect_prompt(
        &self,
        prompt: &str,
        max_tokens: u32,
        server: Option<&mut LlamaServer>,
        suspects: &[(usize, String, String)],
    ) -> std::collections::HashMap<usize, String> {
        let outcome = server
            .and_then(|server| server.complete(prompt, max_tokens))
            .or_else(|| self.run_prompt(prompt, max_tokens));
        let Some(CompletionOutcome {
            content,
            stopped_by_limit,
        }) = outcome
        else {
            return Default::default();
        };
        let expected: std::collections::HashSet<usize> =
            suspects.iter().map(|(index, _, _)| *index).collect();
        let mut result = Self::parse_batch_response(&content, &expected);
        let order: Vec<usize> = suspects.iter().map(|(index, _, _)| *index).collect();
        if let Some(index) = drop_truncated_tail(&mut result, &order, stopped_by_limit) {
            warn!(
                index,
                "反思重试输出被长度上限截断，丢弃末条（该句按回退处理）"
            );
        }
        result
    }

    /// 把一批润色结果写回对应片段。
    ///
    /// 唯一硬约束：**拿不到新结果或新结果为空时，保留原有 `seg.polished`，
    /// 绝不清空、绝不降级为原文**。此前写回是
    /// `results.get(..).cloned().unwrap_or_else(|| seg.text.clone())`：第二遍
    /// 润色（再次点击「润色」，或某批只解析出部分条目）时，没拿到结果的段落
    /// 会被无标点原文覆盖——上一遍的润色成果被静默丢掉，`display_text` 随之
    /// 回退，界面与导出一起退化。
    fn apply_polish_results(
        segments: &mut [Segment],
        results: &std::collections::HashMap<usize, String>,
    ) {
        for seg in segments.iter_mut() {
            match results.get(&seg.index) {
                Some(pol) if !pol.trim().is_empty() => seg.polished = pol.clone(),
                _ => warn!(
                    index = seg.index,
                    "Qwen 未返回该段润色结果（或结果为空），保留原有润色文本，不降级为原文"
                ),
            }
        }
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
        self.store_rewrite_stats(RewriteStats::default());
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
        //
        // 术语表超窗时**只在本次提示词里**裁剪（`fit_glossary_to_context` 是纯函数，
        // 绝不改用户配置），且每次任务只 warn 一次；连裁都放不下就报可读错误，不再让
        // 请求必然超窗、最后被笼统地报成「未能译出任何一句」。
        let user_glossary = self.glossary_hint();
        let glossary = match fit_glossary_to_context(
            &user_glossary,
            self.ctx_size,
            PROMPT_HINT_TOKENS,
        ) {
            Some(fit) => {
                if fit.trimmed {
                    warn!(
                        before_entries = fit.before_entries,
                        after_entries = fit.after_entries,
                        before_chars = fit.before_chars,
                        after_chars = fit.after_chars,
                        ctx_size = self.ctx_size,
                        "术语表过长已超出上下文窗口，本次提示词按优先级裁剪（保留靠前条目；仅影响本次翻译，不改动用户配置）"
                    );
                }
                fit.text
            }
            None => {
                return Err(anyhow!(
                    "术语表过长：术语表 + 提示词固定开销约 {} token，已超过 llm_ctx={} 的可用上下文，无法翻译。请减少术语条目（或调小 max_glossary_entries），或在「性能设置」里调大 llm_ctx。",
                    fixed_prompt_tokens(&user_glossary) + PROMPT_HINT_TOKENS,
                    self.ctx_size
                ));
            }
        };
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
        // 输出校验自愈的统计（回退 / 反思次数），收尾写回引擎供上层提示。
        let mut heal = RewriteStats::default();
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
            // 「改动过大」的可疑句（长度失控 / 语义漂移）：先反思重试一次，仍不合格才留空。
            let mut suspect_candidates: Vec<usize> = Vec::new();
            copy.checked += chunk.len();
            // 校验口径与润色链路一致：`checked` = 本批实际产出、送交校验的条数。
            heal.checked += results.len();
            for &pos in &chunk {
                match results.get(&segments[pos].index) {
                    Some(trans) => {
                        // P0-B + 输出校验：小模型「整体摆烂」时会一字不差地复制原文，
                        // 也偶发把一句扩写成一整段。两者都绝不能当成成功译文写回——
                        // 那正是「界面显示翻译完成、实际什么都没翻」的元凶。
                        // 复制（Echo）走**已有的折半重试**路径；改动过大
                        // （SuspectTooDifferent）走反思重试，仍不合格则保持未译。
                        match validate_translation(&segments[pos].translate_source(), trans) {
                            RewriteVerdict::Echo => {
                                heal.echo += 1;
                                copy_candidates.push(pos);
                                missed.push(pos);
                            }
                            RewriteVerdict::SuspectTooDifferent => {
                                suspect_candidates.push(pos);
                            }
                            // Empty：解析后基本不会出现（空正文已被解析器拒收），
                            // 真出现就按「本批没译出」走折半重试。
                            RewriteVerdict::Empty => {
                                heal.empty += 1;
                                missed.push(pos);
                            }
                            RewriteVerdict::Ok => {
                                segments[pos].translation = Some(trans.clone());
                                segments[pos].translation_lang = Some(target_lang.to_string());
                            }
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
                let retry_rejects = self.retry_missing(
                    &mut segments,
                    &missed,
                    target_lang,
                    &glossary,
                    server.as_mut(),
                );
                // 折半重试里被判「近似复制」而拒收的句子也必须计入复制率，否则一条
                // 近似复制就能绕过 `is_alarming` 兜底（主路径的复制句此前已被计入）。
                // 与主路径的候选合并去重，避免同一句被两段逻辑各计一次。
                copy_candidates.extend(retry_rejects);
                copy_candidates.sort_unstable();
                copy_candidates.dedup();
            }

            // 反思重试（**最多一次**）：只把「改动过大」的可疑句重问一遍，要求最小
            // 必要改动；拿回结果后再校验一遍，仍不合格就保持未译（与既有「未翻译」
            // 口径一致），绝不把可疑译文写进字幕。
            if !suspect_candidates.is_empty() {
                let triples: Vec<(usize, String, String)> = suspect_candidates
                    .iter()
                    .map(|&pos| {
                        (
                            segments[pos].index,
                            segments[pos].translate_source(),
                            results
                                .get(&segments[pos].index)
                                .cloned()
                                .unwrap_or_default(),
                        )
                    })
                    .collect();
                heal.reflect_calls += 1;
                let healed =
                    self.translate_reflect(&triples, target_lang, &glossary, server.as_mut());
                for &pos in &suspect_candidates {
                    let source = segments[pos].translate_source();
                    match healed.get(&segments[pos].index) {
                        Some(text) if validate_translation(&source, text) == RewriteVerdict::Ok => {
                            segments[pos].translation = Some(text.clone());
                            segments[pos].translation_lang = Some(target_lang.to_string());
                        }
                        _ => heal.fallback += 1,
                    }
                }
                warn!(
                    suspects = triples.len(),
                    fallback = heal.fallback,
                    "翻译输出校验自愈：{} 句改动过大，反思重试后仍不合格，保持未译",
                    heal.fallback
                );
            }
            // 重试之后仍没译出的复制句，才真正计入复制率。
            copy.rejected += count_untranslated_copies(&segments, &copy_candidates, target_lang);

            completed += chunk.len();
            if let Some(ref cb) = progress_cb {
                let progress = completed as f64 / pending_total.max(1) as f64;
                cb(
                    progress,
                    &format!("Qwen 批量翻译中: {completed}/{pending_total} 句（共 {total} 句）"),
                );
            }
        }

        // 校验自愈统计先写回引擎：即便下面因质量异常提前返回 Err，上层也能拿到口径。
        self.store_rewrite_stats(heal);
        if heal.has_selfheal() {
            if let Some(ref cb) = progress_cb {
                cb(1.0, &heal.warning_message());
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
    ///
    /// 返回「被判近似复制而拒收」的句子下标，供上层计入复制率。
    fn retry_missing(
        &self,
        segments: &mut [Segment],
        missed: &[usize],
        target_lang: &str,
        glossary: &str,
        mut server: Option<&mut LlamaServer>,
    ) -> Vec<usize> {
        let mut todo: Vec<usize> = missed.to_vec();
        let mut rejected_copy: Vec<usize> = Vec::new();
        // 每轮把待补清单**对半**拆成更小的批（单条时 half=1，即「单句重试一次」）。
        // `guard` 只防极端情况下的死循环；正常 24 条最多 5 轮就收敛。
        let mut guard = 0;
        while !todo.is_empty() && guard < 8 {
            guard += 1;
            let half = todo.len().div_ceil(2);
            let mut still_missing: Vec<usize> = Vec::new();
            for sub in todo.chunks(half) {
                if self.is_cancelled() {
                    return rejected_copy;
                }
                let batch: Vec<Segment> = sub.iter().map(|&p| segments[p].clone()).collect();
                let results =
                    self.translate_batch(&batch, target_lang, glossary, server.as_deref_mut());
                for &pos in sub {
                    match results.get(&segments[pos].index) {
                        Some(trans) => {
                            match classify_retry_outcome(&segments[pos].translate_source(), trans) {
                                RetryVerdict::Accept => {
                                    segments[pos].translation = Some(trans.clone());
                                    segments[pos].translation_lang = Some(target_lang.to_string());
                                }
                                RetryVerdict::RejectCopy => {
                                    // 近似复制：保持未译，并把下标交回上层计入复制率。
                                    rejected_copy.push(pos);
                                    still_missing.push(pos);
                                }
                                RetryVerdict::RejectOther => still_missing.push(pos),
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
        rejected_copy
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
        // 两条路径都能给出截断信号：常驻服务看响应里的 `stop_type: "limit"`，
        // 命令行回退看 stderr 性能统计里生成阶段的 `N runs`（见
        // [`cli_stderr_reports_limit`]）。
        let outcome = server
            .and_then(|server| server.complete(&prompt, max_tokens))
            .or_else(|| self.run_prompt(&prompt, max_tokens));
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
        let order: Vec<usize> = source_segments.iter().map(|s| s.index).collect();
        if let Some(idx) = drop_truncated_tail(&mut result, &order, stopped_by_limit) {
            warn!(
                index = idx,
                batch = source_segments.len(),
                "Qwen 翻译输出被长度上限截断，丢弃末条并交由折半重试补齐"
            );
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
        let outcome = server
            .and_then(|server| server.complete(&prompt, max_tokens))
            .or_else(|| self.run_prompt(&prompt, max_tokens));
        // P1：必须解构出 `stopped_by_limit`。此前这里是
        // `let output = output.content;`，把截断信号直接丢了——模型撞上
        // `n_predict` 上限时返回的是**被切一半的半截文本**，而末行
        // `[3] 半句…` 仍能通过 `parse_batch_response`，于是被当成完整润色
        // 结果写回。
        let Some(CompletionOutcome {
            content: output,
            stopped_by_limit,
        }) = outcome
        else {
            return Default::default();
        };

        Self::polish_batch_finalize(&source_segments, &output, stopped_by_limit)
    }

    /// `polish_batch` 的纯解析半段：解析模型输出，并按截断信号丢弃末条。
    ///
    /// 与 `translate_batch` 的截断处理对齐：信号为真就丢弃**最后一条已解析
    /// 结果**，由上层「保留原文」兜底。但**不复用** `retry_missing`——它用
    /// `validate_translation`（翻译口径）判断成功，而润色后与原文相同完全正常
    /// （只加标点或本就无需改动），会被误判成「没润色」而反复重试 / 丢结果。
    fn polish_batch_finalize(
        source_segments: &[&Segment],
        output: &str,
        stopped_by_limit: bool,
    ) -> std::collections::HashMap<usize, String> {
        let expected = source_segments
            .iter()
            .map(|seg| seg.index)
            .collect::<std::collections::HashSet<_>>();
        let mut result = Self::parse_batch_response(output, &expected);
        let order: Vec<usize> = source_segments.iter().map(|seg| seg.index).collect();
        if let Some(idx) = drop_truncated_tail(&mut result, &order, stopped_by_limit) {
            warn!(
                index = idx,
                batch = source_segments.len(),
                "Qwen 润色输出被长度上限截断，丢弃末条结果并保留原文"
            );
        }
        if result.len() != source_segments.len() {
            warn!(
                "Qwen 润色批量输出不完整 ({}/{} 条)，未匹配条目保留原文",
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

    /// 正文是否含「实义字符」（字母 / 数字 / CJK 表意字…）。
    ///
    /// 只由标点组成的片段（`。`、`…`、`——`）不是有效结果：它只可能来自解析时的
    /// 错切，写回字幕就是一句「只剩标点的译文」。宁可拒收（该句保持未译 / 保留
    /// 原文），也不把垃圾写进字幕。
    fn has_substantive_text(text: &str) -> bool {
        text.chars().any(|c| c.is_alphanumeric())
    }

    /// 去掉块首的分隔装饰（`:`、`-`、`|`、markdown 的 `*` 等）与首尾空白，
    /// 再清掉模型多吐的结束标签。
    fn clean_indexed_body(raw: &str) -> String {
        Self::strip_stop_tags(
            raw.trim()
                .trim_start_matches([
                    ':', '：', '-', '—', '|', '。', '，', '、', '）', ')', '.', '*', '#',
                ])
                .trim(),
        )
    }

    /// 把一段文本按**行内 `[序号]` 标记**切成 `(序号, 正文)` 块。
    ///
    /// 为什么需要它：提示词要求逐行输出，但小模型偶发把两三条并进同一行
    /// （`[1] 甲。 [2] 乙。`）。行级正则只认行首那个序号，`(.+?)$` 会把**后面那几条
    /// 连同它们的 `[2]` 标记一起塞进前一条的正文**——用户看到的就是「第 1 句译文里
    /// 莫名多了第 2 句」。这里按标记把正文切开，每条各自成块，再由调用方用
    /// `expected` 过滤。
    ///
    /// 分块边界判据：标记之前必须是行首、空白或标点。`见参考文献[3]的证明` 里的
    /// `[3]` 紧贴汉字（引用标注，不是分块），**不算边界**——否则会把前一句译文的
    /// 尾巴错切给第 3 句，正是要避免的「张冠李戴」。
    fn split_inline_indexed_blocks(line: &str, re_marker: &regex::Regex) -> Vec<(usize, String)> {
        let mut marks: Vec<(usize, usize, usize)> = Vec::new();
        for caps in re_marker.captures_iter(line) {
            let (Some(m), Some(index)) = (
                caps.get(0),
                caps.get(1).and_then(|c| c.as_str().parse::<usize>().ok()),
            ) else {
                continue;
            };
            let at_boundary = line[..m.start()]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric());
            if at_boundary {
                marks.push((index, m.start(), m.end()));
            }
        }

        let mut blocks = Vec::with_capacity(marks.len());
        for (i, &(index, _, end)) in marks.iter().enumerate() {
            let stop = marks.get(i + 1).map(|m| m.1).unwrap_or(line.len());
            blocks.push((index, Self::clean_indexed_body(&line[end..stop])));
        }
        blocks
    }

    /// 一条解析结果的准入判据，三条缺一不可：
    /// 1. 序号必须在**本批**待处理集合里（`expected`）——防模型改写 / 自己重新编号；
    /// 2. 正文必须含实义字符——纯标点碎片不是译文；
    /// 3. 正文里不得再出现本批**其它序号**的 `[n]` 标记——那说明模型把多条并进了
    ///    一行（或正文里带了引用标注），切分点无从判断；写回就可能张冠李戴。
    ///
    /// 拒收的代价只是「这一句这一轮没写回」：翻译链路会走折半重试（小批通常吐得出
    /// 干净的逐行结果），润色链路则保留上一遍润色/原文。**宁可漏认，不可错认**。
    fn batch_entry_is_trustworthy(
        index: usize,
        text: &str,
        expected: &std::collections::HashSet<usize>,
        re_marker: Option<&regex::Regex>,
    ) -> bool {
        if !expected.contains(&index) || !Self::has_substantive_text(text) {
            return false;
        }
        let Some(re) = re_marker else {
            return true;
        };
        !re.captures_iter(text).any(|caps| {
            caps.get(1)
                .and_then(|c| c.as_str().parse::<usize>().ok())
                .is_some_and(|n| n != index && expected.contains(&n))
        })
    }

    pub(crate) fn parse_batch_response(
        raw: &str,
        expected: &std::collections::HashSet<usize>,
    ) -> std::collections::HashMap<usize, String> {
        let mut result = std::collections::HashMap::new();
        // 1. 去除代码块包裹
        let clean = raw.trim();
        let clean = if let Some(stripped) = clean.strip_prefix("```markdown") {
            stripped.trim_end_matches("```").trim()
        } else if let Some(stripped) = clean.strip_prefix("```") {
            stripped.trim_end_matches("```").trim()
        } else {
            clean
        };

        // 2. 行内 `[序号]` 标记：一行可能塞了多条，先切块再逐块过滤。
        let re_marker = regex::Regex::new(r"\[\s*(\d+)\s*\]").ok();
        // 3. 无方括号的容错写法（`123.` / `123)` / `123、` / `123:`），兼容
        //    Markdown 列表与加粗。
        let re_numbered = regex::Regex::new(
            r"(?m)^\s*(?:[-*•>#\s]*)\s*(?:\*\*)?(\d+)(?:\*\*)?\s*[\.\)）\:：、\-—\|]\s*(.+?)\s*$",
        )
        .ok();

        for line in clean.lines() {
            let line_trim = line.trim();
            if line_trim.is_empty() {
                continue;
            }

            // 2) 行内 `[序号]`（兼容 `[ 3 ]`、`**[3]**`、列表前缀等）。
            if let Some(ref re) = re_marker {
                let mut inserted = false;
                for (index, text) in Self::split_inline_indexed_blocks(line_trim, re) {
                    if Self::batch_entry_is_trustworthy(index, &text, expected, Some(re)) {
                        result.insert(index, text);
                        inserted = true;
                    }
                }
                if inserted {
                    continue;
                }
            }

            // 3) 无方括号的容错写法。
            let mut numbered_matched = false;
            if let Some(ref re) = re_numbered {
                if let Some(caps) = re.captures(line_trim) {
                    if let (Some(m_idx), Some(m_text)) = (caps.get(1), caps.get(2)) {
                        if let Ok(index) = m_idx.as_str().parse::<usize>() {
                            let text = Self::strip_stop_tags(m_text.as_str().trim());
                            if Self::batch_entry_is_trustworthy(
                                index,
                                &text,
                                expected,
                                re_marker.as_ref(),
                            ) {
                                result.insert(index, text);
                                numbered_matched = true;
                            }
                        }
                    }
                }
            }

            // 4) 兜底：前导数字 + 分隔标点（`2. 译文`）。
            if !numbered_matched {
                if let Some((index, text)) = Self::parse_indexed_line(line_trim) {
                    let text = Self::strip_stop_tags(&text);
                    if Self::batch_entry_is_trustworthy(index, &text, expected, re_marker.as_ref())
                    {
                        result.insert(index, text);
                    }
                }
            }
        }

        // 5. 全文兜底扫描：行级解析仍有缺失时按标记再扫一遍。只取标记后的**第一行**
        //    （与旧行为一致）：跨行拼接会把后文的散文吞进正文，得不偿失。
        if result.len() < expected.len() {
            if let Some(ref re) = re_marker {
                for (index, text) in Self::split_inline_indexed_blocks(clean, re) {
                    let text = text.lines().next().unwrap_or("").trim().to_string();
                    if !result.contains_key(&index)
                        && Self::batch_entry_is_trustworthy(index, &text, expected, Some(re))
                    {
                        result.insert(index, text);
                    }
                }
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
        batch_char_budget, chinese_variant_target, clamp_ctx_size, clamp_threads,
        classify_retry_outcome, cli_stderr_reports_limit, cli_wait_budget,
        count_untranslated_copies, drop_truncated_tail, fit_glossary_to_context,
        fixed_overhead_exceeds_window, fixed_prompt_tokens, glossary_entry_count,
        is_untranslated_copy, llama_stop_type_is_limit, output_token_budget,
        plan_translate_batches, similarity, trim_glossary_to_tokens, validate_rewrite,
        validate_translation, CopyRate, LLMEngine, RetryVerdict, RewriteVerdict, MAX_LLM_THREADS,
        MAX_SEGMENTS_PER_BATCH, MIN_BATCH_CHARS, MIN_LLM_THREADS, OUTPUT_TOKENS_PER_CHAR,
        PROMPT_HINT_TOKENS, PROMPT_TOKENS_PER_CHAR,
    };
    use crate::subtitle::Segment;
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

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

    // ─────────── 回填对齐：一行塞多条 / 引用标注 / 纯标点碎片 ───────────

    /// 回归：模型把两条并到同一行时，**不得**把后一条连同它的 `[2]` 标记一起塞进
    /// 前一条的正文。此前行级正则 `(.+?)$` 会整段吞下，第 1 句译文里出现第 2 句
    /// （用户看到的就是「句子张冠李戴」）。
    #[test]
    fn merged_line_splits_blocks_instead_of_leaking_into_previous() {
        let expected = HashSet::from([1, 2]);
        let result = LLMEngine::parse_batch_response("[1] 第一句。 [2] 第二句！", &expected);
        assert_eq!(result.get(&1).map(String::as_str), Some("第一句。"));
        assert_eq!(result.get(&2).map(String::as_str), Some("第二句！"));
        assert!(
            !result.get(&1).unwrap().contains("[2]"),
            "序号标记不得并进前一条正文: {result:?}"
        );
    }

    /// 一行塞三条：每条各自归位，正文互不串味。
    #[test]
    fn merged_line_splits_every_block() {
        let expected = HashSet::from([1, 2, 3]);
        let result = LLMEngine::parse_batch_response("[1] 甲。 [2] 乙。 [3] 丙。", &expected);
        assert_eq!(result.len(), 3, "三条都要解析出来: {result:?}");
        assert_eq!(result.get(&1).map(String::as_str), Some("甲。"));
        assert_eq!(result.get(&2).map(String::as_str), Some("乙。"));
        assert_eq!(result.get(&3).map(String::as_str), Some("丙。"));
    }

    /// 引用标注（`参考文献[3]`）里的 `[3]` 紧贴汉字，不是分块边界：绝不能把第 1 句
    /// 译文的尾巴当第 3 句写回。带这种标注的块一律**拒收**（保守），
    /// 交给折半重试补，不会错位。
    #[test]
    fn glued_reference_marker_is_not_a_block_boundary() {
        let expected = HashSet::from([1, 3]);
        let only_ref = LLMEngine::parse_batch_response("[1] 见参考文献[3]的证明。", &expected);
        assert!(
            !only_ref.contains_key(&3),
            "引用标注不得被当成第 3 句的译文: {only_ref:?}"
        );

        // 同一行里另有真正的 `[3] ` 分块：第 3 句拿到自己的译文，带标注的块保守拒收。
        let mixed = LLMEngine::parse_batch_response("[1] 见参考文献[3]的证明。[3] 丙。", &expected);
        assert_eq!(mixed.get(&3).map(String::as_str), Some("丙。"));
        assert!(
            !mixed.contains_key(&1),
            "带引用标注的块保守拒收，不得错位写回: {mixed:?}"
        );
    }

    /// 纯标点碎片不是译文：既不得写回第 2 句，也不得并进第 1 句。
    #[test]
    fn punctuation_only_fragment_is_rejected() {
        let expected = HashSet::from([1, 2]);
        let result = LLMEngine::parse_batch_response("[1] 甲。 [2] 。", &expected);
        assert_eq!(result.get(&1).map(String::as_str), Some("甲。"));
        assert!(!result.contains_key(&2), "纯标点碎片不得写回: {result:?}");
    }

    // ─────────── 数值边界：上下文窗口 / 命令行等待上限 ───────────

    /// 离谱的 `llm_ctx`（0 / 极大）必须在构造时夹到可用区间——`-c 0` 会让 llama.cpp
    /// 直接拒绝启动（常驻服务与命令行回退双双失败）。
    #[test]
    fn ctx_size_is_clamped_to_usable_range() {
        assert_eq!(clamp_ctx_size(0), 512);
        assert_eq!(clamp_ctx_size(1), 512);
        assert_eq!(clamp_ctx_size(4096), 4096);
        assert_eq!(clamp_ctx_size(u32::MAX), 65_536);
        // 引擎必须拿夹过的窗口去算预算；线程数下限同样是 1。
        let engine = LLMEngine::new("cli", "model.gguf", 0, 0);
        assert_eq!(engine.ctx_size, 512);
        assert_eq!(engine.threads(), 1);
    }

    /// 命令行回退路径的等待上限：随 token 预算增长，且有下限/上限——既不能
    /// 「无限等」（子进程挂住就整条流水线卡死），也不能小到把正常生成掐掉。
    #[test]
    fn cli_wait_budget_is_bounded_and_grows_with_tokens() {
        assert_eq!(cli_wait_budget(0), Duration::from_secs(60));
        assert_eq!(cli_wait_budget(64), Duration::from_secs(60));
        assert_eq!(cli_wait_budget(1_000), Duration::from_secs(500));
        assert_eq!(cli_wait_budget(u32::MAX), Duration::from_secs(900));
        assert!(cli_wait_budget(1_000) > cli_wait_budget(200));
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

    // ─────────── P1：术语表固定开销超过上下文窗口 ───────────

    /// 「固定开销本身超过可用窗口」必须被判定函数识别，且预算函数如实返回 0
    /// （旧实现用 `.max(64)` 把**必然超窗**伪装成「预算正常（64 字符）」）。
    #[test]
    fn fixed_overhead_over_window_is_detected() {
        let ctx = 4096; // 默认 `llm_ctx`
                        // 默认配置量级：80 条 × (60+60) 字符 ≈ 9.7k 字符 ≈ 8.2k token，已是整个窗口的两倍。
        let default_glossary = "甲".repeat(80 * 122);
        let fixed = fixed_prompt_tokens(&default_glossary) + PROMPT_HINT_TOKENS;
        assert!(
            fixed_overhead_exceeds_window(ctx, fixed),
            "超窗必须被判定出来"
        );
        assert_eq!(
            batch_char_budget(ctx, fixed),
            0,
            "超窗时预算必须如实归零，不得伪装成 64"
        );
        // 正常规模不触顶
        assert!(!fixed_overhead_exceeds_window(ctx, 0));
        assert!(batch_char_budget(ctx, 0) > 0);
    }

    /// 超窗时按优先级裁剪术语表：保留表头与**靠前**词条、裁掉尾部；裁剪后必须真的
    /// 落回窗口内；且只影响本次提示词，用户术语表原样不动（纯函数）。
    #[test]
    fn glossary_is_trimmed_to_fit_context_without_touching_config() {
        let ctx = 4096;
        let glossary = "术语表（以下词条必须按给定译法翻译，不得改写）：".to_string()
            + &(0..800)
                .map(|i| format!("term{i}=术语{i}"))
                .collect::<Vec<_>>()
                .join("；")
            + "。";
        let original = glossary.clone();
        let fit =
            fit_glossary_to_context(&glossary, ctx, PROMPT_HINT_TOKENS).expect("必须能裁进窗口");
        assert!(fit.trimmed);
        assert!(fit.after_chars < fit.before_chars);
        assert!(fit.after_entries < fit.before_entries);
        assert!(fit.after_entries >= 1);
        // 优先级：表头 + 首条保留，尾部被裁掉
        assert!(fit.text.starts_with("术语表（"));
        assert!(fit.text.contains("term0=术语0"));
        assert!(!fit.text.contains("term799"));
        // 裁完后确实落在窗口内，且仍留得下一批最小输入
        let fixed = fixed_prompt_tokens(&fit.text) + PROMPT_HINT_TOKENS;
        assert!(!fixed_overhead_exceeds_window(ctx, fixed));
        assert!(batch_char_budget(ctx, fixed) >= MIN_BATCH_CHARS);
        // 纯函数：输入（用户配置）原样未动
        assert_eq!(glossary, original);
    }

    /// 真的一点都放不下时返回 `None`，交由上层报可读错误；空术语表永远放得下。
    #[test]
    fn glossary_fit_returns_none_when_nothing_fits() {
        assert!(
            fit_glossary_to_context("术语表：a=b", 512, 400).is_none(),
            "固定开销已吃掉整窗口时必须返回 None"
        );
        let empty = fit_glossary_to_context("", 512, 0).expect("空表应放得下");
        assert!(!empty.trimmed);
        assert_eq!(empty.after_entries, 0);
        assert_eq!(empty.after_chars, 0);
    }

    /// `trim_glossary_to_tokens` 边界：空表恒可放；预算为 0 且非空 → `None`；
    /// 预算足够 → 原样返回；预算极小 → 退化为前缀截断而非空。
    #[test]
    fn trim_glossary_token_budget_boundaries() {
        assert_eq!(trim_glossary_to_tokens("", 0), Some(String::new()));
        assert_eq!(trim_glossary_to_tokens("术语表：a=b", 0), None);
        let g = "术语表：a=b；c=d。";
        assert_eq!(trim_glossary_to_tokens(g, 1_000), Some(g.to_string()));
        assert_eq!(glossary_entry_count(g), 2);
        assert_eq!(glossary_entry_count(""), 0);
        let tiny = trim_glossary_to_tokens(g, 3).expect("小预算也应给出前缀");
        assert!(!tiny.is_empty());
        assert!(tiny.chars().count() < g.chars().count());
    }

    /// 输出预算在真的没空间时如实归零（不再 `.max(128)` 伪装），正常/紧张时仍给足。
    #[test]
    fn output_budget_is_honest_when_context_is_exhausted() {
        let ctx = 4096;
        let relaxed = output_token_budget(1_000, ctx, 0);
        let tight = output_token_budget(1_000, ctx, 2_000);
        assert!(relaxed >= 128);
        assert!(tight >= 128 && tight < relaxed);
        assert_eq!(
            output_token_budget(1_000, ctx, 8_000),
            0,
            "固定开销超窗时如实返回 0，由上层拦截"
        );
    }

    /// 线程数必须同时夹**上界**（同级引擎都这么做），不能只 `max(1)`。
    #[test]
    fn threads_are_clamped_to_sane_range() {
        assert_eq!(clamp_threads(0), MIN_LLM_THREADS);
        assert_eq!(clamp_threads(1), 1);
        assert_eq!(clamp_threads(8), 8);
        assert_eq!(clamp_threads(u32::MAX), MAX_LLM_THREADS);
        let engine = LLMEngine::new("cli", "model.gguf", 4096, u32::MAX);
        assert_eq!(engine.threads(), MAX_LLM_THREADS);
        engine.set_threads(u32::MAX);
        assert_eq!(engine.threads(), MAX_LLM_THREADS, "set_threads 也要夹上界");
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

    // ─────────── P2：命令行回退路径的截断信号（stderr 性能统计） ───────────

    /// 生成阶段 `N runs` 达到 `-n` 上限：命令行回退路径也必须报「被截断」。
    #[test]
    fn cli_stderr_reports_limit_when_runs_reach_max_tokens() {
        let stderr = "llama_perf_context_print:        eval time =   100.00 ms /   512 runs   ( 0.20 ms per token, 5000.00 tokens per second)";
        assert!(cli_stderr_reports_limit(stderr, 512));
    }

    /// `N runs` 未达上限：模型是自然收尾的，不能当成截断（误判会丢掉完整译文）。
    #[test]
    fn cli_stderr_reports_limit_ignores_runs_below_max_tokens() {
        let stderr = "llama_perf_context_print:        eval time =   100.00 ms /   100 runs   ( 1.00 ms per token, 1000.00 tokens per second)";
        assert!(!cli_stderr_reports_limit(stderr, 512));
    }

    /// `prompt eval time` 行的 `N runs` 是**输入** token 数（远大于 max_tokens），
    /// 混进来会把每次请求都误判成截断。
    #[test]
    fn cli_stderr_reports_limit_ignores_prompt_eval_line() {
        let stderr = "llama_perf_context_print: prompt eval time =  10.00 ms /  9000 runs   ( 0.00 ms per token, 900000.00 tokens per second)";
        assert!(!cli_stderr_reports_limit(stderr, 512));
    }

    /// 没有统计（版本差异 / `--no-perf`）时宁可漏报：不能把完整输出误判成截断。
    #[test]
    fn cli_stderr_reports_limit_false_on_unknown_log() {
        assert!(!cli_stderr_reports_limit("some random log", 512));
        assert!(!cli_stderr_reports_limit("", 512));
        // max_tokens == 0：没有上限可比，直接不判截断
        assert!(!cli_stderr_reports_limit(
            "llama_perf_context_print:        eval time =   100.00 ms /   512 runs   ( 0.20 ms per token)",
            0
        ));
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

    /// `drop_truncated_tail`：无截断信号时一条都不丢；有截断信号时按 `order` 倒序
    /// 丢弃**最后一条已解析**（半截末条被丢、完整末条不被丢靠的就是这个开关）。
    #[test]
    fn drop_truncated_tail_keeps_or_drops_last_parsed_line() {
        let mut complete: HashMap<usize, String> =
            HashMap::from([(1, "第一句。".to_string()), (3, "第三句。".to_string())]);
        let order = [1usize, 2, 3];
        // 无截断信号：完整的末条必须原样保留
        assert_eq!(drop_truncated_tail(&mut complete, &order, false), None);
        assert!(complete.contains_key(&3), "无截断信号不得丢弃完整末条");
        assert_eq!(complete.len(), 2);
        // 有截断信号：序号 2 没有结果，倒序第一个命中的 3 才是「最后一条已解析」
        assert_eq!(drop_truncated_tail(&mut complete, &order, true), Some(3));
        assert_eq!(complete.len(), 1);
        assert!(complete.contains_key(&1));

        // 只有一条时同理
        let mut single: HashMap<usize, String> = HashMap::from([(1, "半截".to_string())]);
        assert_eq!(drop_truncated_tail(&mut single, &[1usize], true), Some(1));
        assert!(single.is_empty());

        // 结果为空：没有可丢的，返回 None
        let mut empty: HashMap<usize, String> = HashMap::new();
        assert_eq!(drop_truncated_tail(&mut empty, &order, true), None);
    }
    // ─────────── P1：润色链路的截断丢弃与结果保留 ───────────

    /// 撞上 `n_predict` 上限时，被截断的末条润色结果必须被丢弃（半截文本不得
    /// 写回），其余段落照常写回；没有截断信号时同样的输出必须全部保留。
    #[test]
    fn 润色截断时丢弃末条结果且其余段落仍写回() {
        let engine = LLMEngine::new(
            "definitely-not-a-real-cli-path.exe",
            "definitely-not-a-real-model.gguf",
            4096,
            4,
        );
        let segs = [
            seg_at(1, "今天讲概率不等式"),
            seg_at(2, "下一节讲条件概率"),
            seg_at(3, "最后讲贝叶斯公式"),
        ];
        // 末行 `[3] 最后` 是被 `n_predict` 切一半的半截文本：解析器认得出它，
        // 但绝不能当成完整润色结果。
        let raw = "[1] 今天讲概率不等式。\n[2] 下一节讲条件概率。\n[3] 最后";
        // 无截断信号：三条都保留，不能误丢
        let refs: Vec<&Segment> = segs.iter().collect();
        let complete = LLMEngine::polish_batch_finalize(&refs, raw, false);
        assert_eq!(complete.len(), 3, "无截断时不得丢弃任何条目");
        assert_eq!(complete.get(&3).map(String::as_str), Some("最后"));

        // 有截断信号：末条被丢弃，其余两条保留
        let truncated = LLMEngine::polish_batch_finalize(&refs, raw, true);
        assert_eq!(truncated.len(), 2, "截断时必须丢弃末条: {truncated:?}");
        assert!(!truncated.contains_key(&3), "被截断的末条不得写回");
        assert_eq!(
            truncated.get(&1).map(String::as_str),
            Some("今天讲概率不等式。")
        );
        assert_eq!(
            truncated.get(&2).map(String::as_str),
            Some("下一节讲条件概率。")
        );

        // 写回：被丢弃的那条保留原文（不得写半截文本），其余两条写回润色结果
        let mut target = vec![
            seg_at(1, "今天讲概率不等式"),
            seg_at(2, "下一节讲条件概率"),
            seg_at(3, "最后讲贝叶斯公式"),
        ];
        engine.polish_apply(&mut target, None, |_| truncated.clone());
        assert_eq!(target[0].polished, "今天讲概率不等式。");
        assert_eq!(target[1].polished, "下一节讲条件概率。");
        assert!(
            target[2].polished.trim().is_empty(),
            "被截断的段落不得写回半截文本"
        );
        assert_eq!(
            target[2].display_text(),
            "最后讲贝叶斯公式",
            "截断段落应回退原文"
        );
    }

    /// 第二遍润色没拿到结果（或结果为空）时，第一遍的润色结果必须原样保留，
    /// 不得被无标点原文覆盖降级（此前 `unwrap_or_else(|| seg.text.clone())`
    /// 会把上一遍成果直接抹掉）。
    #[test]
    fn 润色未返回结果时保留上一遍润色() {
        let engine = LLMEngine::new(
            "definitely-not-a-real-cli-path.exe",
            "definitely-not-a-real-model.gguf",
            4096,
            4,
        );
        let mut segs = vec![seg_at(1, "今天讲概率不等式"), seg_at(2, "下一节讲条件概率")];

        // 第一遍：两段都拿到润色结果
        let first = HashMap::from([
            (1usize, "今天讲概率不等式。".to_string()),
            (2usize, "下一节讲条件概率。".to_string()),
        ]);
        engine.polish_apply(&mut segs, None, |_| first.clone());
        assert_eq!(segs[0].polished, "今天讲概率不等式。");
        assert_eq!(segs[1].polished, "下一节讲条件概率。");

        // 第二遍：只返回第 1 段；第 2 段拿到的是空白串（模拟模型漏段 / 空结果）
        let second = HashMap::from([
            (1usize, "今天讲概率不等式，是重点。".to_string()),
            (2usize, "   ".to_string()),
        ]);
        engine.polish_apply(&mut segs, None, |_| second.clone());
        assert_eq!(segs[0].polished, "今天讲概率不等式，是重点。");
        assert_eq!(
            segs[1].polished, "下一节讲条件概率。",
            "空白结果不得清空上一遍的润色"
        );

        // 第三遍：整批都没拿到（空映射）——上一遍结果仍须完整保留
        engine.polish_apply(&mut segs, None, |_| HashMap::new());
        assert_eq!(segs[0].polished, "今天讲概率不等式，是重点。");
        assert_eq!(segs[1].polished, "下一节讲条件概率。");
        assert_eq!(segs[0].display_text(), "今天讲概率不等式，是重点。");
        assert_eq!(
            segs[1].display_text(),
            "下一节讲条件概率。",
            "display_text 不得回退成原文"
        );
    }

    // ─────────── 输出校验自愈：相似度 ───────────

    /// 完全相同（含「只差大小写 / 标点」）→ 1.0。
    #[test]
    fn similarity_identical_is_one() {
        assert!((similarity("今天讲概率不等式。", "今天讲概率不等式。") - 1.0).abs() < 1e-9);
        assert!((similarity("Hello, World!", "hello world") - 1.0).abs() < 1e-9);
    }

    /// 没有任何共享实义字符 → 0.0。
    #[test]
    fn similarity_completely_different_is_zero() {
        assert!(similarity("概率不等式", "abcxyz").abs() < 1e-9);
        assert!(similarity("苹果香蕉", "unrelated latin words").abs() < 1e-9);
    }

    /// 空 / 纯标点的边界：两边都空 → 1.0；仅一边空 → 0.0。
    #[test]
    fn similarity_empty_boundaries() {
        assert_eq!(similarity("", ""), 1.0);
        assert_eq!(similarity("   。，", "..."), 1.0);
        assert_eq!(similarity("", "内容"), 0.0);
        assert_eq!(similarity("内容", ""), 0.0);
    }

    /// 仅标点差异（补标点 / 全半角混排）不算「改内容」，相似度仍是 1.0。
    #[test]
    fn similarity_punctuation_only_difference_is_full() {
        assert!((similarity("今天讲概率不等式", "今天讲概率不等式。") - 1.0).abs() < 1e-9);
        assert!((similarity("P(A+B)=P(A)+P(B)", "p(a+b) = p(a) + p(b)") - 1.0).abs() < 1e-9);
    }

    /// 中英混排：部分重叠应给出 (0,1) 之间的中间值。
    #[test]
    fn similarity_mixed_cjk_latin_partial() {
        let s = similarity("确认OK，本节讲3点", "确认OK，本节讲5点");
        assert!(s > 0.5 && s < 1.0, "中英混排部分重叠应给中间值: {s}");
    }

    /// 超长输入按 SIMILARITY_MAX_CHARS 截断：共同前缀很长时仍判「几乎相同」。
    #[test]
    fn similarity_truncates_very_long_input() {
        let head = "字".repeat(5_000);
        let a = format!("{head}甲");
        let b = format!("{head}乙");
        let s = similarity(&a, &b);
        assert!(s > 0.99, "超长共同前缀应判为几乎相同: {s}");
        assert!((s - 1.0).abs() < 0.02, "尾部差异在截断窗口外: {s}");
    }

    // ─────────── 输出校验自愈：判定与回退 ───────────

    /// `validate_rewrite` 的四种判定都要能触发。
    #[test]
    fn validate_rewrite_reports_four_verdicts() {
        let src = "今天讲概率不等式，其实考得不多";
        // Ok：补标点 + 轻微改词
        assert_eq!(
            validate_rewrite(src, "今天讲概率不等式，其实考得并不多。"),
            RewriteVerdict::Ok
        );
        // Echo：归一化后与原文一致（模型没干活）
        assert_eq!(
            validate_rewrite(src, "今天讲概率不等式，其实考得不多。"),
            RewriteVerdict::Echo
        );
        // SuspectTooDifferent：整句被重写
        assert_eq!(
            validate_rewrite(src, "完全不同的另一句话内容"),
            RewriteVerdict::SuspectTooDifferent
        );
        // Empty：空白输出
        assert_eq!(validate_rewrite(src, "   "), RewriteVerdict::Empty);
    }

    /// 翻译校验：复制原文判 Echo、正常长句英译放行、长度失控判可疑、短句原样保留放行。
    #[test]
    fn validate_translation_detects_copy_and_runaway() {
        let src = "那么这个概率不等式，其实整体里面考的相对来说不是很多。";
        assert_eq!(validate_translation(src, src), RewriteVerdict::Echo);
        assert_eq!(
            validate_translation(
                src,
                "This probability inequality is not tested very much overall."
            ),
            RewriteVerdict::Ok
        );
        assert_eq!(validate_translation(src, "  "), RewriteVerdict::Empty);
        let runaway = "这是一段".to_string() + &"扩写内容".repeat(200);
        assert_eq!(
            validate_translation("这句只有十几个字的一句话", &runaway),
            RewriteVerdict::SuspectTooDifferent
        );
        // 短句 / 纯数字：本就该原样保留，不得误判为「没干活」
        assert_eq!(validate_translation("OK", "OK"), RewriteVerdict::Ok);
        assert_eq!(validate_translation("1.3", "1.3"), RewriteVerdict::Ok);
    }

    /// 回退与统计：可疑句被剔除、Echo 在润色场景被采纳，计数准确。
    #[test]
    fn selfheal_falls_back_suspect_and_counts() {
        let mut originals = HashMap::new();
        originals.insert(1usize, "今天讲概率不等式其实考得不多".to_string());
        originals.insert(2usize, "下一节讲条件概率".to_string());
        let first = HashMap::from([
            (1usize, "完全不同的另一句话内容".to_string()),
            (2usize, "下一节讲条件概率。".to_string()),
        ]);
        let (accepted, stats) =
            LLMEngine::selfheal_batch(validate_rewrite, true, &originals, first, |_suspects| {
                HashMap::new()
            });
        assert_eq!(stats.checked, 2);
        assert_eq!(stats.echo, 1, "补标点属于 Echo");
        assert_eq!(stats.fallback, 1, "可疑句应从结果中剔除");
        assert_eq!(stats.reflect_calls, 1, "可疑句存在时应发起一次反思");
        assert!(accepted.contains_key(&2), "Echo 在润色场景视为合格结果");
        assert!(!accepted.contains_key(&1), "回退项不得写回");
    }

    /// 反思重试**最多一次**：假模型调用计数必须恰好为 1，且仍不合格就回退。
    #[test]
    fn selfheal_reflect_runs_at_most_once() {
        let mut originals = HashMap::new();
        originals.insert(7usize, "今天讲概率不等式其实考得不多".to_string());
        let first = HashMap::from([(7usize, "一句毫不相干的怪话".to_string())]);
        let calls = std::cell::Cell::new(0usize);
        let (accepted, stats) =
            LLMEngine::selfheal_batch(validate_rewrite, true, &originals, first, |_suspects| {
                calls.set(calls.get() + 1);
                // 反思结果**仍然**改动过大 → 必须回退，且不得再重试
                HashMap::from([(7usize, "还是另一句毫不相干的话".to_string())])
            });
        assert_eq!(calls.get(), 1, "反思只允许重试一次");
        assert_eq!(stats.reflect_calls, 1);
        assert_eq!(stats.fallback, 1);
        assert!(accepted.is_empty(), "反思仍不合格时必须回退");
    }

    /// 润色链路端到端：可疑润色回退成原文，正常补标点照常写回。
    #[test]
    fn polish_selfheal_falls_back_suspect_to_original() {
        let engine = LLMEngine::new(
            "definitely-not-a-real-cli-path.exe",
            "definitely-not-a-real-model.gguf",
            4096,
            4,
        );
        let mut segs = vec![seg_at(1, "今天讲概率不等式"), seg_at(2, "下一节讲条件概率")];
        let bad = HashMap::from([
            (1usize, "一段完全不相干的内容重新写".to_string()),
            (2usize, "下一节讲条件概率。".to_string()),
        ]);
        let mut reflect_calls = 0usize;
        let stats = engine.polish_apply_selfheal(
            &mut segs,
            None,
            |_batch| bad.clone(),
            |_suspects| {
                reflect_calls += 1;
                HashMap::new()
            },
        );
        assert_eq!(reflect_calls, 1, "反思只重试一次");
        assert_eq!(stats.fallback, 1);
        assert_eq!(segs[0].polished, "", "可疑润色不得写回");
        assert_eq!(segs[0].display_text(), "今天讲概率不等式", "回退后显示原文");
        assert_eq!(segs[1].polished, "下一节讲条件概率。", "正常润色照常写回");
    }

    // ─────────── P1：折半重试写回必须与主路径同一条校验 ───────────

    /// 折半重试写回前的判定：只有 `Ok` 才写回；近似复制（改一个功能字）判
    /// `RejectCopy`（保持未译并计入复制率），长度失控 / 空输出判 `RejectOther`。
    #[test]
    fn retry_write_back_shares_main_path_validation() {
        let source = "今天的天气非常好我们一起去公园散步吧".repeat(2);
        // 近似复制：只改了最后一个字，相似度 ≥ 0.97 但不是完全相等。
        let near_copy = format!("{}呀", &source[..source.len() - "吧".len()]);
        assert!(
            !is_untranslated_copy(&source, &near_copy),
            "旧判据（只判完全相等）会放过近似复制"
        );
        let sim = similarity(&source, &near_copy);
        assert!(
            sim >= 0.97,
            "测试前提：近似复制的相似度须落在 Echo 区间: {sim}"
        );
        assert_eq!(
            classify_retry_outcome(&source, &near_copy),
            RetryVerdict::RejectCopy
        );
        // 长度失控（长句被压成两个字）
        assert_eq!(
            classify_retry_outcome(&source, "好的"),
            RetryVerdict::RejectOther
        );
        // 空输出
        assert_eq!(
            classify_retry_outcome(&source, "   "),
            RetryVerdict::RejectOther
        );
        // 正常译文照旧写回
        assert_eq!(
            classify_retry_outcome(
                &source,
                "Lovely weather today, let us walk in the park together."
            ),
            RetryVerdict::Accept
        );
        // 原有放行守卫：短句 / 纯公式「译文==原文」本就正确
        assert_eq!(classify_retry_outcome("OK", "OK"), RetryVerdict::Accept);
    }

    /// 折半重试被判「近似复制」而保持未译的句子必须计入复制率；重试补回正常译文的
    /// 句子不得计入（否则会把好句子算成复制、抬高复制率）。
    #[test]
    fn retry_rejected_copies_count_toward_copy_rate() {
        let mut segs = vec![
            seg_at(1, "第一句源文本需要翻译"),
            seg_at(2, "第二句源文本需要翻译"),
        ];
        // 句 2 折半重试后补回了正常译文；句 1 仍是「未译出」。
        segs[1].translation = Some("The second sentence.".to_string());
        segs[1].translation_lang = Some("English".to_string());
        let candidates = vec![0usize, 1];
        assert_eq!(
            count_untranslated_copies(&segs, &candidates, "English"),
            1,
            "只有仍未译出的复制句计入"
        );
        // 折半重试拒收的复制句被并进候选后，复制率过半必须触发兜底告警
        let copy = CopyRate {
            rejected: 3,
            checked: 4,
        };
        assert!(copy.is_alarming());
    }
}
