//! 性能设置工作台视图
//!
//! 实现「硬件检测 → 性能评估 → 策略矩阵决策 → 一键应用」的原生 GPUI 交互面板。
//! 此功能为实验性功能。

use gpui::prelude::*;
use gpui::*;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use std::sync::Mutex;
use std::time::Duration;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;
use crate::utils::config::{ModelInfo, TranslateConfig};

/// 在线翻译「每批条数」滑块的取值区间。
///
/// 下限 1（逐条翻译，最保守，适合限流严格的接口）、上限 50（再大单次请求容易
/// 撞上模型上下文上限或触发 429）。原先只给 10/20/40 三个固定档，用户想试
/// 25 条这种中间值没有任何入口——批量大小和接口限流、模型上下文、字幕平均
/// 长度都相关，本来就该连续可调。
const TRANSLATE_BATCH_MIN: u32 = 1;
const TRANSLATE_BATCH_MAX: u32 = 50;

/// 术语表**实际生效**的条数（纯函数，便于单测）。
///
/// `TranslateConfig::glossary_entries()` 返回解析出的全部合法条目，而真正注入
/// 提示词的只有前 `TranslateConfig::MAX_GLOSSARY_ENTRIES` 条（见
/// `utils::config::glossary_prompt` 的 `.take(..)`）。界面直接显示解析条数
/// 会让用户以为全部生效——这里统一按实际生效数展示，超限时另行提示。
pub(crate) fn effective_glossary_count(parsed: usize, limit: usize) -> usize {
    parsed.min(limit)
}

/// 术语表**解析条数**的跨帧缓存：`glossary_entries()` 会把整段文本解析成
/// `Vec<(String, String)>`（每个合法条目两次分配），而设置卡每帧都要显示这个数字。
///
/// 键 = 「原文长度 + 原文哈希」：原文一改键就变，不会显示陈旧条数；键不变时只付一次
/// 哈希（零分配），省掉每帧一次整表解析。放在进程级静态量里：`MainWindow` 字段定义在
/// `ui/mod.rs`（本模块无权改），与 [`MODEL_LIST`] 同一套「只在主线程触碰 + 中毒可用」写法。
static GLOSSARY_COUNT: Mutex<Option<((usize, u64), usize)>> = Mutex::new(None);

/// 术语表解析出的条目数（带跨帧缓存，见 [`GLOSSARY_COUNT`]）。
fn glossary_count_cached(cfg: &TranslateConfig) -> usize {
    let mut hasher = DefaultHasher::new();
    cfg.glossary.hash(&mut hasher);
    let key = (cfg.glossary.len(), hasher.finish());

    let mut slot = match GLOSSARY_COUNT.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let cached = slot
        .as_ref()
        .filter(|(cached_key, _)| *cached_key == key)
        .map(|(_, count)| *count);
    if let Some(count) = cached {
        return count;
    }
    let count = cfg.glossary_entries().len();
    *slot = Some((key, count));
    count
}

/// 离线模型路径行的状态提示：把「手动编辑的校验结果」与「`qwen-llm` 是否就位」
/// 两路信号合成一条 `(是否中性色, 文案)`。
///
/// 抽成纯函数是为了单测——真正的三态耦合（校验红字 vs. 就位绿字）在界面里没法
/// 用断言表达，而这段规则恰恰是最容易改错、也最容易自相矛盾的地方（例如
/// 「绿字说路径有效、旁边却挂着下载按钮」）。规则：
/// - `hint = Some((false, msg))`：手动编辑过且校验失败 → **最高优先级**透传红字原因。
///   哪怕此刻正在下载也不能盖掉它：下载的是内置默认模型，而配置里的路径仍然是错的，
///   下完照样用不了（`LLMEngine` 读的就是 `paths.llm_model`）。让红字与旁边的
///   「恢复默认」按钮同时在场，才是这一情形真正的出路。
/// - `hint = Some((true, msg))` 且模型已就位 → 透传中性确认；
/// - 下载中 → 中性「正在下载模型…」（避免下载期间还挂着「未就位」误导成没反应）；
/// - 其余 → 看就位与否给「未就位，怎么办」或「已就位，还能换」。
pub(crate) fn local_model_status(
    hint: Option<(bool, String)>,
    model_present: bool,
    downloading: bool,
) -> (bool, String) {
    match hint {
        Some((false, msg)) => (false, msg),
        Some((true, msg)) if model_present => (true, msg),
        _ if downloading => (true, "正在下载模型…".to_string()),
        _ if model_present => (true, "模型已就位；也可指向你自己下载的 .gguf".to_string()),
        _ => (
            false,
            "模型未就位：填一个 .gguf 路径，或点「下载模型」用内置 Qwen".to_string(),
        ),
    }
}

// ==================== 在线翻译：模型列表拉取 ====================

/// 「拉取模型列表」的结果缓存。
///
/// 为什么不挂在 `MainWindow` 上：本文件的改动范围只到 `performance.rs`，`ui/mod.rs`
/// 里的窗口字段不能动。列表是进程级的临时数据（拉一次用一阵），放静态量不影响正确性：
/// 渲染时按 `(api_base, api_key)` 指纹比对，配置一改旧列表立即作废，不会拿过期结果
/// 去覆盖用户新填的基址。
static MODEL_LIST: Mutex<Option<ModelListCache>> = Mutex::new(None);
static MODEL_CATEGORY: Mutex<String> = Mutex::new(String::new());

struct ModelListCache {
    /// 拉取时的配置指纹（基址 + 生效密钥），用于判断列表是否已过期
    fingerprint: String,
    /// 拉取中：按钮显示「拉取中…」并置灰，避免连点发多轮请求
    loading: bool,
    /// `(是否成功, 文案)`，`None` = 还没拉过
    status: Option<(bool, String)>,
    /// 服务端返回的模型（顺序与响应一致）
    models: Vec<ModelInfo>,
}

/// 模型列表是否已过期：基址或密钥变了，旧列表就不能再往配置里写。
pub(crate) fn model_cache_fingerprint(cfg: &TranslateConfig) -> String {
    format!("{}|{}", cfg.api_base.trim(), cfg.effective_api_key().trim())
}

/// 加锁访问缓存；锁中毒（渲染线程 panic 后）也继续用，不把一次 panic 升级成永久空白。
fn with_model_cache<R>(f: impl FnOnce(&mut Option<ModelListCache>) -> R) -> R {
    match MODEL_LIST.lock() {
        Ok(mut guard) => f(&mut guard),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// 取当前配置下的缓存快照 `(loading, status, models)`；指纹不匹配一律当作「没拉过」。
fn model_cache_snapshot(cfg: &TranslateConfig) -> (bool, Option<(bool, String)>, Vec<ModelInfo>) {
    let fingerprint = model_cache_fingerprint(cfg);
    with_model_cache(|slot| match slot.as_ref() {
        Some(cache) if cache.fingerprint == fingerprint => {
            (cache.loading, cache.status.clone(), cache.models.clone())
        }
        _ => (false, None, Vec::new()),
    })
}
/// 取当前配置下的缓存**元信息** `(loading, status)`，不克隆模型列表。
///
/// `model_cache_snapshot` 连整个 `Vec<ModelInfo>` 一起深拷贝，而「获取模型列表」行
/// 只用 loading / status：列表长（网关可返回上百条）时，逐帧白拷一份纯属浪费。
fn model_cache_status(cfg: &TranslateConfig) -> (bool, Option<(bool, String)>) {
    let fingerprint = model_cache_fingerprint(cfg);
    with_model_cache(|slot| match slot.as_ref() {
        Some(cache) if cache.fingerprint == fingerprint => (cache.loading, cache.status.clone()),
        _ => (false, None),
    })
}

/// 在当前配置下**借用**模型列表调用 `f`；指纹不匹配一律按「没拉过」（`None`）。
///
/// 与 [`model_cache_snapshot`] 的区别是**不深拷贝** `Vec<ModelInfo>`：「可用模型池」
/// 在在线档下逐帧渲染，而网关能返回上百条（每条还带 `name` / 上下文长度等字段），
/// 每帧为取几个 id 白拷一整份表是纯浪费。锁只在本（主）线程被触碰，持锁构建元素
/// 不会与谁争用。
fn with_fresh_model_cache<R>(
    cfg: &TranslateConfig,
    f: impl FnOnce(Option<&ModelListCache>) -> R,
) -> R {
    let fingerprint = model_cache_fingerprint(cfg);
    with_model_cache(|slot| {
        let fresh = match slot.as_ref() {
            Some(cache) if cache.fingerprint == fingerprint => Some(cache),
            _ => None,
        };
        f(fresh)
    })
}

/// 拉取模型列表的失败信息。`raw` 是 `HTTP {code}: {响应体}` 原文，用来匹配
/// [`crate::utils::config::online_error_hint`]。
struct ModelFetchError {
    /// 有状态码时带上，`None` 表示连不上（DNS / 拒绝连接）
    status: Option<u16>,
    /// 响应体里 `error.message` / `gateway_hint` 摘出来的可读说明
    detail: String,
    /// 原始文案（供错误提示匹配）
    raw: String,
}

impl ModelFetchError {
    /// 兜底：连不上或响应体读不出来时用。
    fn transport(message: String) -> Self {
        Self {
            status: None,
            detail: message.clone(),
            raw: message,
        }
    }
}

/// 拉取成功后的结果。
struct ModelFetchOutcome {
    models: Vec<ModelInfo>,
    /// 实际命中的 URL
    url: String,
    /// 需要额外提醒用户的事（如「基址少了 /v1」）
    note: Option<String>,
}

/// 响应体片段：出错时贴给用户看，太长就截断（密钥不会出现在 `/models` 响应里）。
fn body_snippet(body: &str) -> String {
    let snippet: String = body.chars().take(300).collect();
    if snippet.trim().is_empty() {
        "（空响应体）".to_string()
    } else {
        snippet
    }
}

/// 把「拉取失败」拼成一句可操作的话：**原样**带上网关的 `error.message` /
/// `gateway_hint`（只显示「HTTP 400」等于什么都没说），再附上针对性提示。
pub(crate) fn model_fetch_error_message(status: Option<u16>, detail: &str, raw: &str) -> String {
    let mut msg = match status {
        Some(code) => format!("拉取失败（HTTP {code}）：{detail}"),
        None => format!("拉取失败：{detail}"),
    };
    if let Some(hint) = crate::utils::config::online_error_hint(raw) {
        msg.push('\n');
        msg.push_str("提示：");
        msg.push_str(&hint);
    }
    msg
}

/// 拉取成功的状态文案（含「基址少了 /v1」这类提醒）。
pub(crate) fn model_fetch_success_message(url: &str, count: usize, note: Option<&str>) -> String {
    let mut msg = format!("已从 {url} 拉取到 {count} 个模型，点击即写入模型名");
    if let Some(note) = note {
        msg.push('\n');
        msg.push_str(note);
    }
    msg
}

/// 拉取成功后的收尾：解析响应体（解析失败 / 空列表都算失败，界面要说清原因）。
fn finish_model_fetch(
    body: String,
    url: String,
    note: Option<String>,
) -> Result<ModelFetchOutcome, String> {
    let models = crate::utils::config::parse_model_list(&body).map_err(|e| {
        format!(
            "{url} 的响应无法解析：{e}\n响应片段：{}",
            body_snippet(&body)
        )
    })?;
    if models.is_empty() {
        return Err(format!(
            "{url} 返回了 0 个模型（服务端未开放任何模型）\n响应片段：{}",
            body_snippet(&body)
        ));
    }
    Ok(ModelFetchOutcome { models, url, note })
}

/// GET `{api_base}/models`（带 Bearer 密钥）。非 2xx 也返回 `Err`，由调用方决定是否重试。
fn http_get_models(url: &str, key: &str, timeout_secs: u64) -> Result<String, ModelFetchError> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout_secs.clamp(5, 60)))
        .build();
    let response = agent
        .get(url)
        .set("Authorization", &format!("Bearer {}", key.trim()))
        .call();
    match response {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| ModelFetchError::transport(format!("读取 {url} 的响应失败：{e}"))),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let detail = crate::utils::config::gateway_error_summary(&body)
                .unwrap_or_else(|| body_snippet(&body));
            Err(ModelFetchError {
                status: Some(code),
                detail,
                raw: format!("在线接口返回 HTTP {code}：{body}"),
            })
        }
        Err(e) => Err(ModelFetchError::transport(format!("无法连接 {url}：{e}"))),
    }
}

/// 拉取模型列表：先打归一化后的 `/models`；若 404 且基址没带 `/v1`，再试一次
/// `{基址}/v1/models`（实测网关只认带 `/v1` 的路径），命中后顺带提示补全基址。
///
/// 全过程是阻塞式 HTTP，调用方必须放进 `background_executor`（见
/// [`MainWindow::fetch_translate_models`]）。
fn fetch_model_list(cfg: &TranslateConfig, key: &str) -> Result<ModelFetchOutcome, String> {
    let primary = cfg.models_url();
    if primary.is_empty() {
        return Err("请先填写「接口基址」（例如 http://host:3021/v1）".to_string());
    }
    let err = match http_get_models(&primary, key, cfg.timeout_secs) {
        Ok(body) => return finish_model_fetch(body, primary, None),
        Err(err) => err,
    };
    if err.status == Some(404) {
        if let Some(alt) = cfg.models_url_v1_fallback() {
            if let Ok(body) = http_get_models(&alt, key, cfg.timeout_secs) {
                let base = cfg.api_base.trim().trim_end_matches('/');
                let note = format!(
                    "注意：基址缺少 /v1（本次用 {alt} 拉取成功），建议把「接口基址」改成 {base}/v1"
                );
                return finish_model_fetch(body, alt, Some(note));
            }
        }
    }
    Err(model_fetch_error_message(err.status, &err.detail, &err.raw))
}

/// 并行进程数滑条的量程上限：按当前识别引擎区分。
///
/// 两条转写路径的硬上限是两个不同的常量——Whisper 夹 `MAX_WORKERS`(4)、
/// SenseVoice 夹 `MAX_SV_WORKERS`(6)。抽成纯函数是为了让单测能直接钉住
/// 「滑条上限 == 引擎实际会夹到的上限」这一等式：
/// - 取 Whisper 的 4 给 SenseVoice：界面选不到 5/6，而引擎本可跑满 6；
/// - 取 SenseVoice 的 6 给 Whisper：界面显示 5/6、引擎静默夹到 4。
///
/// 两种都是「改了没效果」的观感来源。
fn parallel_process_max(is_sensevoice: bool) -> u32 {
    let hard_max = if is_sensevoice {
        crate::engines::chunked_whisper::MAX_SV_WORKERS
    } else {
        crate::engines::chunked_whisper::MAX_WORKERS
    };
    (hard_max as u32).max(1)
}

/// 转写线程数滑条的量程下限：SenseVoice 与 Whisper 最左档都取 2（与引擎线程下限一致）。
fn thread_slider_min(is_sensevoice: bool) -> u32 {
    if is_sensevoice {
        crate::engines::sensevoice::MIN_THREADS
    } else {
        2
    }
}

/// 转写线程数滑条的量程上限：按当前识别引擎区分。
///
/// Whisper 侧没有单会话线程上限（引擎原样接受任意线程数），滑条给到逻辑核数即可；
/// SenseVoice 的单会话线程数在引擎里夹到 `sensevoice::MAX_THREADS`(8)。此前滑条量程
/// 一律取逻辑核数：16 核机器上用户能选到 9~16，界面与 config.toml 都记着 16，引擎却
/// 按 8 跑——与并行进程数、VAD 阈值同型的「改了没效果」。这条函数把「滑条上限 ==
/// 引擎实际会夹到的上限」钉成单测可断言的不等式。
fn thread_slider_max(is_sensevoice: bool, cores: u32) -> u32 {
    let hard_max = if is_sensevoice {
        crate::engines::sensevoice::MAX_THREADS
    } else {
        u32::MAX
    };
    cores.max(2).min(hard_max)
}

/// VAD 阈值滑条的整数档位：先把配置值夹进引擎有效区间，再换算成百分数。
///
/// 渲染器只吃 u32；此前是「先取整再夹」，异常配置（例如 0.15）会被记成 15 档，
/// 与实际生效的 0.30 对不上。先夹后取整后，滑条上的每个档位都真实生效。
fn vad_slider_value(vad: f64) -> u32 {
    use crate::engines::whisper::{VAD_THRESHOLD_MAX, VAD_THRESHOLD_MIN};
    (vad.clamp(VAD_THRESHOLD_MIN, VAD_THRESHOLD_MAX) * 100.0).round() as u32
}

/// 「测试连接」按钮的文案：探测进行中要说清「正在探测」，别让按钮看起来仍可点击
/// （重复点击会连发多轮网络请求）。
///
/// 抽成纯函数的理由与一旁的滑条换算相同：文案判定进不了界面单测，而它恰好决定
/// 「按钮此刻代表哪个状态」——这里写反了，用户就分不清该不该再点一次。
fn probe_button_label(probing: bool) -> &'static str {
    if probing {
        "测试中…"
    } else {
        "测试连接"
    }
}

/// 「可用模型池」内嵌滚动网格的跨帧状态（只为一个滚动句柄）。
///
/// 为什么放在 GPUI 全局而不是 `MainWindow` 字段：字段定义在 `ui/mod.rs`（本模块无权
/// 改动），而滚动句柄必须**跨帧是同一个**——每帧新建一个，`track_scroll` 读到的偏移
/// 永远归零，滚轮判据也随之失效。全局随 App 生命周期存活，等价于视图级状态。
struct ModelPoolScroll {
    handle: ScrollHandle,
}

impl Default for ModelPoolScroll {
    fn default() -> Self {
        Self {
            handle: ScrollHandle::new(),
        }
    }
}

impl Global for ModelPoolScroll {}

/// 本机可用模型的跨帧快照：扫描 `models/whisper/` 的结果 + 每个文件的体积。
///
/// # 为什么必须有这个快照（而不是渲染时现扫）
///
/// 渲染路径**不做 I/O** 是本项目的硬约定（见 `views/library.rs` 的
/// `ensure_library_sizes` 范式）。而「列出本机有哪些模型」天然是一次 `read_dir`
/// 加每个文件一次 `metadata`——菜单打开期间每帧都跑一遍是明确的违规。这里照同一
/// 范式：键变了才在后台扫一次，渲染只读结果。
///
/// 存在 GPUI 全局而不是 `MainWindow` 字段里：字段定义在 `ui/mod.rs`，
/// 本模块不扩它的面。
#[derive(Default)]
struct LocalModelsSnapshot {
    /// 快照基于哪个键算出（`None` = 还没算过）
    key: Option<ModelsKey>,
    /// 扫描结果（已排序，故顺序稳定）
    models: Vec<crate::utils::local_models::LocalModel>,
    /// 后台扫描进行中：避免每帧重复派发
    inflight: bool,
}

impl Global for LocalModelsSnapshot {}

/// 本机已下载的**离线大模型**（`models/llm/*.gguf`）的跨帧快照。
///
/// 与 [`LocalModelsSnapshot`] 同一个范式、同一个理由（渲染路径零 I/O），
/// 只是扫的是另一个目录、且不需要档位概念。失效键直接复用 [`ModelsKey`]。
#[derive(Default)]
struct LlmModelsSnapshot {
    key: Option<ModelsKey>,
    models: Vec<crate::utils::local_models::LocalModel>,
    inflight: bool,
}

impl Global for LlmModelsSnapshot {}

/// 模型快照的失效键（判定抽成纯函数 [`models_snapshot_stale`]）。
///
/// # 为什么用「目录 mtime」而不是「逐条目存在性」
///
/// 旧实现（`TierDiskKey`）键的是 `ITEMS` 各条目的就位与否——它只能感知**清单里
/// 登记过**的文件变化。而本次要支持的恰恰是清单外的自备模型：用户往目录里丢一个
/// `.bin`，逐条目判定完全看不出来。目录 mtime 对「新增 / 删除 / 改名」都会变，
/// 一次 `metadata` 就能覆盖两类来源（下载完成 与 手工放置）。
#[derive(Clone, PartialEq, Eq)]
struct ModelsKey {
    /// 下载在跑：下载完成 true→false 是「立刻刷新」信号（完成回调在别的文件里）
    downloading: bool,
    /// 模型目录的修改时间；目录不存在时为 `None`
    dir_mtime: Option<(u64, u32)>,
}

/// 模型快照是否需要重扫（纯函数，便于单测）：键变了 → 重扫；键不变 → 复用。
fn models_snapshot_stale(cached: Option<&ModelsKey>, current: &ModelsKey) -> bool {
    cached != Some(current)
}

/// 读**大模型目录**的失效键（与 [`models_key`] 同构，只是换目录）。
fn llm_models_key(downloading: bool) -> ModelsKey {
    let dir_mtime = std::fs::metadata(crate::utils::local_models::llm_models_dir())
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), d.subsec_nanos()));
    ModelsKey {
        downloading,
        dir_mtime,
    }
}

/// 读模型目录的失效键。只做一次 `metadata`。
fn models_key(downloading: bool) -> ModelsKey {
    let dir_mtime = std::fs::metadata(crate::utils::local_models::models_dir())
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), d.subsec_nanos()));
    ModelsKey {
        downloading,
        dir_mtime,
    }
}

impl MainWindow {
    /// 渲染性能设置工作台
    pub(crate) fn render_performance_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_tab = self.settings_tab;

        // 顶部子选项卡切换条
        let tab_bar = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .mb(px(Theme::SPACE_3))
            .child(
                div()
                    .id("settings-subtab-hardware")
                    .px(px(Theme::SPACE_3))
                    .py(px(Theme::SPACE_1_5))
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(if current_tab == 0 {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .bg(if current_tab == 0 {
                        Theme::accent_mint()
                    } else {
                        Theme::bg_card()
                    })
                    .text_color(if current_tab == 0 {
                        Theme::text_on_accent()
                    } else {
                        Theme::text_secondary()
                    })
                    .border_1()
                    .border_color(if current_tab == 0 {
                        Theme::accent_mint()
                    } else {
                        Theme::border_subtle()
                    })
                    .hover(|s| {
                        if current_tab != 0 {
                            s.bg(Theme::bg_hover()).text_color(Theme::text_primary())
                        } else {
                            s
                        }
                    })
                    .child("硬件与推理")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings_tab = 0;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("settings-subtab-llm")
                    .px(px(Theme::SPACE_3))
                    .py(px(Theme::SPACE_1_5))
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(if current_tab == 1 {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .bg(if current_tab == 1 {
                        Theme::accent_mint()
                    } else {
                        Theme::bg_card()
                    })
                    .text_color(if current_tab == 1 {
                        Theme::text_on_accent()
                    } else {
                        Theme::text_secondary()
                    })
                    .border_1()
                    .border_color(if current_tab == 1 {
                        Theme::accent_mint()
                    } else {
                        Theme::border_subtle()
                    })
                    .hover(|s| {
                        if current_tab != 1 {
                            s.bg(Theme::bg_hover()).text_color(Theme::text_primary())
                        } else {
                            s
                        }
                    })
                    .child("翻译与润色")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings_tab = 1;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("settings-subtab-download")
                    .px(px(Theme::SPACE_3))
                    .py(px(Theme::SPACE_1_5))
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(if current_tab == 2 {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .bg(if current_tab == 2 {
                        Theme::accent_mint()
                    } else {
                        Theme::bg_card()
                    })
                    .text_color(if current_tab == 2 {
                        Theme::text_on_accent()
                    } else {
                        Theme::text_secondary()
                    })
                    .border_1()
                    .border_color(if current_tab == 2 {
                        Theme::accent_mint()
                    } else {
                        Theme::border_subtle()
                    })
                    .hover(|s| {
                        if current_tab != 2 {
                            s.bg(Theme::bg_hover()).text_color(Theme::text_primary())
                        } else {
                            s
                        }
                    })
                    .child("模型与组件")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings_tab = 2;
                        cx.notify();
                    })),
            );

        // 自适应响应式内容区：小屏一列、大屏两列自适应
        let content = match current_tab {
            0 => {
                // Tab 0: 硬件与推理加速
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(Theme::SPACE_4))
                    // 左列（只剩硬件加速模式）
                    //
                    // 这里曾有「识别引擎」三元组（识别引擎开关 / Whisper 模型档位下拉 /
                    // ONNX 后端状态行）。模型管理已经集中到「模型与组件」页与右侧抽屉的
                    // 「识别引擎（已就位）」卡，同一件事在三个地方各有一套入口只会互相打架，
                    // 因此整卡移除。选中哪个引擎/模型现在只由那两处决定。
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(380.0))
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_4))
                            .child(self.render_hardware_tier_card(cx)),
                    )
                    // 右列（并行度 + 音频预处理管道）
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(380.0))
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_4))
                            .child(self.render_parallelism_card(cx))
                            .child(self.render_audio_pipeline_card(cx)),
                    )
                    .into_any_element()
            }
            1 => {
                // Tab 1: 翻译与润色配置
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(Theme::SPACE_4))
                    // 左列（翻译引擎配置）
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(380.0))
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_4))
                            .child(self.render_translate_settings_card(cx)),
                    )
                    // 右列（标点与 AI 润色）
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(380.0))
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_4))
                            .child(self.render_polish_selection_card(cx)),
                    )
                    .into_any_element()
            }
            _ => {
                // Tab 2: 组件与模型下载
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_4))
                    .child(self.render_model_manager(false, cx))
                    .child(self.render_project_audit_card(cx))
                    .into_any_element()
            }
        };

        primitives::page_shell("performance-settings-page")
            .flex_1()
            .w_full()
            // 1. 页头：标题 + 帮助按钮
            .child(self.render_page_header(cx))
            // 2. 配置说明卡 (右上角问号按钮展开)
            .children(if self.state.show_perf_help {
                Some(self.render_perf_help_card(cx))
            } else {
                None
            })
            // 3. 子选项卡导航条
            .child(tab_bar)
            // 4. 自适应双列/单列内容
            .child(content)
    }

    /// 硬件加速模式选择卡（我有 GPU / 纯 CPU 模式自由无缝切换）
    fn render_hardware_tier_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_gpu = self.state.config.gpu.is_gpu_tier();

        let tier_control = primitives::segmented_cluster()
            .child(self.seg_option(
                "perf-tier-gpu",
                "独立显卡（GPU 加速）",
                is_gpu,
                cx,
                |this, cx| {
                    this.state.set_hardware_tier(true);
                    cx.notify();
                },
            ))
            .child(self.seg_option(
                "perf-tier-cpu",
                "无独立显卡（纯 CPU 模式）",
                !is_gpu,
                cx,
                |this, cx| {
                    this.state.set_hardware_tier(false);
                    cx.notify();
                },
            ))
            .into_any_element();

        primitives::card_rows().child(
            div()
                .w_full()
                .py(px(Theme::SPACE_2))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(Theme::SPACE_3))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(Theme::SPACE_2))
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_BODY_LG))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_primary())
                                .child("硬件加速模式"),
                        )
                        .child(if is_gpu {
                            primitives::badge_accent("GPU 加速已启用")
                        } else {
                            primitives::badge("纯 CPU 模式")
                        }),
                )
                .child(tier_control),
        )
    }

    /// 步骤 5：字幕翻译引擎设置。
    ///
    /// 离线档用本地 llama.cpp + Qwen，免费且断网可用；在线档走任意 OpenAI 兼容的
    /// `/chat/completions`（DeepSeek / OpenAI / 通义 / Kimi / 本地 vLLM 均可），
    /// 质量与速度都更好，但需要密钥。两档共用一个「测试连接」按钮，
    /// 让用户在整片翻译失败之前就能发现密钥或地址写错。
    fn render_translate_settings_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::engines::TranslateMode;
        use crate::ui::ApiField;

        let mode = self.state.translate_mode;
        let is_online = mode == TranslateMode::OnlineApi;

        let mode_control = primitives::segmented_cluster()
            .child(self.seg_option(
                "perf-translate-offline",
                "本地 Qwen",
                !is_online,
                cx,
                |this, cx| {
                    this.state
                        .set_translate_mode(crate::engines::TranslateMode::OfflineQwen);
                    cx.notify();
                },
            ))
            .child(self.seg_option(
                "perf-translate-online",
                "在线 API",
                is_online,
                cx,
                |this, cx| {
                    this.state
                        .set_translate_mode(crate::engines::TranslateMode::OnlineApi);
                    cx.notify();
                },
            ))
            .into_any_element();

        let base_control = self.render_api_input("api-base-input", ApiField::Base, cx);
        let model_control = self.render_api_input("api-model-input", ApiField::Model, cx);
        let key_input = self.render_api_input("api-key-input", ApiField::Key, cx);
        let key_visible = self.api_key_visible;
        let key_control = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .w_full()
            .child(key_input)
            .child(
                // 「显示 / 隐藏」密钥：外观与 chip 原语同形同色，直接复用
                primitives::chip(if key_visible { "隐藏" } else { "显示" }, false, false)
                    .id("api-key-visible-toggle")
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.api_key_visible = !this.api_key_visible;
                        cx.notify();
                    })),
            )
            .into_any_element();

        // 每批条数：在线接口按 token 计费，批量越大往返越少；本地推理则影响不大。
        //
        // 用**滑块**而不是 10/20/40 三档按钮：最优值和接口限流、模型上下文、
        // 字幕平均长度都相关，三档把「我想试 25 条」这种需求堵死了。
        // 拖动期间只改内存（`begin_config_drag` 闸门拦住落盘），松手由
        // `render_slider` 自带的 on_mouse_up 调 `end_config_drag` 统一写一次。
        let batch = self
            .state
            .config
            .translate
            .batch_size
            .clamp(TRANSLATE_BATCH_MIN as usize, TRANSLATE_BATCH_MAX as usize)
            as u32;
        let batch_control = self
            .render_slider(
                "perf-slider-translate-batch",
                "每批条数",
                batch,
                TRANSLATE_BATCH_MIN,
                TRANSLATE_BATCH_MAX,
                format!("{batch} 条/批"),
                "一次请求携带的字幕条数：越大往返次数越少、越省 token；过大可能撞上下文上限或被限流"
                    .to_string(),
                cx,
                |this, v, cx| {
                    let v = v as usize;
                    if this.state.config.translate.batch_size != v {
                        this.state.config.translate.batch_size = v;
                        this.state.save_translate_config();
                        cx.notify();
                    }
                },
            )
            .into_any_element();

        // 请求超时：慢接口（自建网关、海外代理）常常 30–60 秒才回，默认 120 秒够用，
        // 但弱网 / 长批时需要能调大。此前只能改 config.toml 或用 CLI 的 --api-timeout。
        let timeout_secs = self.state.config.translate.timeout_secs.clamp(10, 600) as u32;
        let timeout_slider = self
            .render_slider(
                "perf-slider-translate-timeout",
                "请求超时",
                timeout_secs,
                10,
                600,
                format!("{timeout_secs} 秒"),
                "单批请求的等待上限：接口慢或批量大时调大，避免整批被当成超时失败".to_string(),
                cx,
                |this, v, cx| {
                    let v = v as u64;
                    if this.state.config.translate.timeout_secs != v {
                        this.state.config.translate.timeout_secs = v;
                        this.state.save_translate_config();
                        cx.notify();
                    }
                },
            )
            .into_any_element();

        let probing = self.is_probing_translate;
        let probe_msg = self.translate_probe_msg.clone();
        let probe_control = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(if let Some((ok, text)) = probe_msg {
                div()
                    .max_w(px(Theme::PROBE_MSG_MAX_W))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(if ok {
                        Theme::accent_mint()
                    } else {
                        Theme::accent_red()
                    })
                    .truncate()
                    .child(text)
                    .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(
                // 外观走 btn_state 原语（尺寸 / 圆角 / 禁用配色全站同源），探测中显示
                // 「测试中…」并置灰：按钮长得还能点、点了却没反应，用户会以为程序卡死，
                // 连点还会连发多轮请求。
                primitives::btn_state(
                    probe_button_label(probing),
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                    !probing,
                )
                .id("perf-translate-probe-btn")
                .when(!probing, |d| {
                    d.on_click(cx.listener(|this, _, _, cx| {
                        this.probe_online_translate_api(cx);
                    }))
                }),
            )
            .into_any_element();

        // 术语表：多行文本，用外部编辑器改（自绘单行框装不下）。两档引擎都生效。
        // 解析出的条目数包含**全部**合法条目，但注入提示词时只取前
        // `TranslateConfig::MAX_GLOSSARY_ENTRIES` 条（见 `utils::config::glossary_prompt`）。
        // 显示解析条数会让用户以为「120 条全在生效」，实际只有前 80 条——
        // 这里按实际生效数显示，并在触到上限时明确提示被截断。
        let glossary_count = glossary_count_cached(&self.state.config.translate);
        let glossary_limit = self.state.config.translate.effective_glossary_limit();
        let glossary_effective = effective_glossary_count(glossary_count, glossary_limit);
        let glossary_status = self.glossary_status.clone();
        let glossary_control = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(if glossary_count > 0 {
                        if glossary_effective < glossary_count {
                            format!(
                                "已启用 {glossary_count} 条术语，其中 {glossary_effective} 条生效（受上限 {glossary_limit} 条限制）"
                            )
                        } else {
                            format!("已启用 {glossary_count} 条术语")
                        }
                    } else {
                        "未设置".to_string()
                    }),
            )
            .child(
                primitives::chip_clickable("编辑术语表", false, false)
                    .id("glossary-edit-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_glossary_editor(cx);
                    })),
            )
            .child(
                primitives::chip_clickable("应用术语表", false, false)
                    .id("glossary-apply-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.apply_glossary_from_file(cx);
                    })),
            )
            // 候选词提取：术语表最大的门槛是「想不起来要加哪些词」。
            // 从当前字幕里把英文缩写 / 驼峰名 / 型号捞出来，以注释形式追加，
            // 用户补上译文即可生效——不自动写入条目，避免污染译文。
            .child(
                primitives::chip_clickable("提取候选词", false, false)
                    .id("glossary-extract-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.extract_glossary_candidates(cx);
                    })),
            )
            // 导入 / 导出：术语表在本页（翻译配置区）才有归属，转写页不重复放一份，
            // 免得两处入口行为不一致。导入走异步文件对话框（`.txt` / `.csv`），
            // 导出复用「另存为 + 后台写盘」那一条出口，这里只挂点击。
            .child(
                primitives::chip_clickable("导入", false, false)
                    .id("glossary-import-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_glossary_dialog(cx);
                    })),
            )
            .child(
                primitives::chip_clickable("导出", false, false)
                    .id("glossary-export-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.export_glossary_csv(cx);
                    })),
            )
            .children(glossary_status.map(|msg| {
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_secondary())
                    .truncate()
                    .child(msg)
            }))
            .into_any_element();

        // 模型选择区：说明文案 + 「拉取模型列表」按钮 + 点击即用的下拉。
        // 用户实测踩到的第一个坑就是模型 id 少了 `cn:` 前缀 → 400 model_unavailable，
        // 所以这一段既给「去哪查 id」的入口，也把「必须完全一致」写在旁边。
        let model_fetch_row = self.render_model_fetch_row(is_online, cx);
        let model_choice_row = if is_online {
            Some(self.render_model_choice_row(cx))
        } else {
            None
        };

        // 离线档下把在线参数整块调暗：仍然可见可编辑（方便提前填好），但明确提示未生效
        let dim = move |el: AnyElement| -> AnyElement {
            if is_online {
                el
            } else {
                div().opacity(0.45).child(el).into_any_element()
            }
        };

        // 离线档的参数（本地模型文件）与在线档参数**调暗方向相反**：
        // 在线档下本地模型路径整行调暗，明确「当前未生效」。
        let dim_offline = move |el: AnyElement| -> AnyElement {
            if is_online {
                div().opacity(0.45).child(el).into_any_element()
            } else {
                el
            }
        };
        let local_model_control = self.render_local_model_row(cx);

        primitives::card_rows()
            .child(Self::render_setting_row("翻译引擎", mode_control))
            .child(Self::render_setting_divider())
            .child(
                // 自己写一行而不是走 render_setting_row：这一行是「输入框 + 按钮 +
                // 下方提示」的纵向组合，塞进「左标签 + 右对齐控件」骨架会被折成窄条
                // （写法参照同文件的 render_onnx_provider_row）。
                div().w_full().py(px(Theme::SPACE_2)).child(dim_offline(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(Theme::SPACE_1_5))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap(px(Theme::SPACE_2))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(Theme::TEXT_BODY_LG))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::text_primary())
                                        .child("本地模型"),
                                )
                                .child(local_model_control),
                        )
                        .into_any_element(),
                )),
            )
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("接口基址", dim(base_control)))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("模型名", dim(model_control)))
            .child(
                // 自己写一行而不是走 render_setting_row：说明文案 + 按钮 + 会换行的
                // 下拉列表塞进「左标签 + 右对齐控件」骨架会被折成右对齐的窄条
                // （写法参照同文件的 render_onnx_provider_row）。
                div().w_full().py(px(Theme::SPACE_2)).child(dim(div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_2))
                    .child(model_fetch_row)
                    .children(model_choice_row)
                    .into_any_element())),
            )
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("API Key", dim(key_control)))
            .child(Self::render_setting_divider())
            // 滑块行自带「标签 + 提示 + 步进 + 轨道」，不再套 render_setting_row
            // （那条骨架是「左标签 + 右对齐控件」，会把整条滑轨压成右侧窄条）。
            .child(dim(batch_control))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("连通性", probe_control))
            .child(Self::render_setting_divider())
            .child(timeout_slider)
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("术语表", glossary_control))
    }

    /// 刷新「本机可用模型」快照：键变化时后台重扫一次（见 [`LocalModelsSnapshot`]）。
    ///
    /// 渲染路径只读内存：键不变就复用，键变（目录内容变了 / 下载完成）才派发一次
    /// 后台扫描，完成后 `cx.notify()` 自动刷新。
    fn ensure_local_models_snapshot(&self, cx: &mut Context<Self>) {
        let key = models_key(self.state.is_downloading);
        let stale = cx.update_default_global::<LocalModelsSnapshot, _>(|state, _| {
            models_snapshot_stale(state.key.as_ref(), &key) && !state.inflight
        });
        if !stale {
            return;
        }
        let key_for_store = key;
        cx.update_default_global::<LocalModelsSnapshot, _>(|state, _| state.inflight = true);
        cx.spawn(async move |this, cx| {
            let models = cx
                .background_executor()
                .spawn(async move { crate::utils::local_models::scan_installed_models() })
                .await;
            let _ = this.update(cx, |_this, cx| {
                cx.update_default_global::<LocalModelsSnapshot, _>(|state, _| {
                    state.models = models;
                    state.key = Some(key_for_store);
                    state.inflight = false;
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 当前快照里的模型列表（渲染路径零 I/O；首帧快照未就绪时为空表）。
    ///
    /// `pub(crate)`：右侧配置抽屉的「识别引擎」卡也用它——两处必须看**同一份**
    /// 扫描结果，否则会出现「抽屉里有、下拉里没有」这种自相矛盾的界面。
    pub(crate) fn local_models_snapshot(
        &self,
        cx: &mut Context<Self>,
    ) -> Vec<crate::utils::local_models::LocalModel> {
        self.ensure_local_models_snapshot(cx);
        cx.update_default_global::<LocalModelsSnapshot, _>(|state, _| state.models.clone())
    }

    /// 刷新「本机离线大模型」快照：键变化时后台重扫一次（见 [`LlmModelsSnapshot`]）。
    fn ensure_llm_models_snapshot(&self, cx: &mut Context<Self>) {
        let key = llm_models_key(self.state.is_downloading);
        let stale = cx.update_default_global::<LlmModelsSnapshot, _>(|state, _| {
            models_snapshot_stale(state.key.as_ref(), &key) && !state.inflight
        });
        if !stale {
            return;
        }
        let key_for_store = key;
        cx.update_default_global::<LlmModelsSnapshot, _>(|state, _| state.inflight = true);
        cx.spawn(async move |this, cx| {
            let models = cx
                .background_executor()
                .spawn(async move { crate::utils::local_models::scan_llm_models() })
                .await;
            let _ = this.update(cx, |_this, cx| {
                cx.update_default_global::<LlmModelsSnapshot, _>(|state, _| {
                    state.models = models;
                    state.key = Some(key_for_store);
                    state.inflight = false;
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 当前快照里的本机大模型列表（渲染路径零 I/O）。
    fn llm_models_snapshot(
        &self,
        cx: &mut Context<Self>,
    ) -> Vec<crate::utils::local_models::LocalModel> {
        self.ensure_llm_models_snapshot(cx);
        cx.update_default_global::<LlmModelsSnapshot, _>(|state, _| state.models.clone())
    }

    /// 工程体检卡：列出当前工程的阶段产物、占用空间与「是否可复用」（指纹）。
    ///
    /// 阶段产物（`.voice2word-stages/<stem>/03_transcribe.json` 等）是续跑缓存：
    /// 参数变了或旧版本留下的产物会被判为过期，但**留在磁盘上**没人看得见。
    /// 这里把它们摊开，并给一个「清理过期产物」的出口。
    fn render_project_audit_card(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::utils::project_audit as pa;
        let Some(audit) = self
            .state
            .selected_file
            .as_deref()
            .map(pa::audit_media)
            .filter(|a| !a.artifacts.is_empty())
        else {
            return div().into_any_element();
        };

        let stale = audit
            .artifacts
            .iter()
            .filter(|a| a.fingerprint_ok != Some(true))
            .count();
        let rows: Vec<AnyElement> = audit
            .artifacts
            .iter()
            .take(8)
            .map(|a| {
                let (label, color) = match a.fingerprint_ok {
                    Some(true) => ("可复用", Theme::accent_mint()),
                    Some(false) => ("过期", Theme::accent_red()),
                    None => ("无法识别", Theme::text_muted()),
                };
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_secondary())
                            .child(format!("{}（{}）", a.stage, pa::human_bytes(a.bytes))),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(color)
                            .child(label),
                    )
                    .into_any_element()
            })
            .collect();

        primitives::card_sm()
            .flex_none()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_primary())
                    .child("工程体检"),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(format!(
                        "阶段产物 {} 个，占用 {}（其中 {} 个过期）",
                        audit.artifacts.len(),
                        pa::human_bytes(audit.total_bytes),
                        stale
                    )),
            )
            .children(rows)
            .child(
                primitives::btn_clickable(
                    "清理过期产物",
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id("perf-audit-clear-stale")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(media) = this.state.selected_file.clone() {
                        let audit = crate::utils::project_audit::audit_media(&media);
                        let cleared = crate::utils::project_audit::clear_stale(&audit);
                        this.notice = Some(format!("已清理 {cleared} 个过期阶段产物"));
                    }
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    /// 页头：标题 + 帮助按钮 (简约，无徽标)
    fn render_page_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let show_help = self.state.show_perf_help;
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .child(primitives::page_title("性能设置"))
            .child(
                div()
                    .id("perf-help-toggle-btn")
                    .w(px(Theme::CTRL_H_SM))
                    .h(px(Theme::CTRL_H_SM))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .bg(if show_help {
                        Theme::tint_blue_badge()
                    } else {
                        Theme::bg_raised()
                    })
                    .border_1()
                    .border_color(if show_help {
                        Theme::tint_blue_border()
                    } else {
                        Theme::border_mid()
                    })
                    .text_size(px(Theme::TEXT_BODY_LG))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if show_help {
                        Theme::accent_blue()
                    } else {
                        Theme::text_secondary()
                    })
                    .hover(|s| {
                        s.border_color(Theme::tint_blue_border())
                            .text_color(Theme::accent_blue())
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.show_perf_help = !this.state.show_perf_help;
                        cx.notify();
                    }))
                    .child("?"),
            )
    }

    /// 推荐配置卡：硬件评估档位 + 用户推理偏好 + 「套用推荐配置」。
    ///
    /// 为什么要单独一张卡：本页帮助卡里宣传的「速度优先 / 平衡模式 / 精度优先，
    /// 一键套用整套推荐配置」此前没有任何入口——`set_user_strategy` /
    /// `apply_recommended_profile` / `performance_level` 只在 `AppState` 里定义，
    /// UI 侧一次都没调用过：用户既看不到自己属于哪一档，也没有按钮能让推荐参数
    /// 真正生效。这里把这条链路补全：选偏好 → 重算策略矩阵 → 写进 config.toml，
    /// 并把「当前档位 / 将要生效的参数 / 是否已偏离推荐」一并回显。
    #[allow(dead_code)]
    fn render_recommend_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::core::UserStrategy;

        let level = self.state.performance_level;
        let strategy = self.state.user_strategy;
        let profile = &self.state.recommended_profile;

        // 套用链路：按偏好重算推荐 → 写进内存与管线 → 刷新模型就位判定 → 落盘。
        // 落盘沿用 actions.rs 的既有模式：改完 state 调 save_to_file，出错进 notice。
        let apply_profile = move |this: &mut Self, cx: &mut Context<Self>, option: UserStrategy| {
            this.state.set_user_strategy(option);
            this.state.apply_recommended_profile();
            // 推荐里的 `gpu_offload` 以前是死字段：矩阵建议开 GPU 转写，套用后
            // config.gpu.whisper_offload 仍是旧值。这里接上唯一消费点——推荐为真
            // 才写开关；推荐为假（只说明没检测到独显）不动用户正在用的核显加速。
            crate::core::performance::consume_gpu_offload_recommendation(
                &mut this.state.config.gpu,
                &this.state.recommended_profile,
            );
            // 推荐档位可能改写 config.paths.whisper_model，下方「模型与组件」卡的
            // 就位徽标缓存要跟着重算，否则会显示成上一档位的结论。
            this.state.refresh_model_presence();
            if let Err(e) = this.state.config.save_to_file("config.toml") {
                this.notice = Some(format!("推荐配置已生效，但写入 config.toml 失败：{e}"));
            }
            cx.notify();
        };

        // 三个偏好档：点选即完成一次「一键套用」（帮助卡承诺的那一下）。
        let strategy_options: [(UserStrategy, &'static str); 3] = [
            (UserStrategy::Speed, "perf-strategy-speed"),
            (UserStrategy::Balanced, "perf-strategy-balanced"),
            (UserStrategy::Quality, "perf-strategy-quality"),
        ];
        let mut strategy_control = primitives::segmented_cluster();
        for (option, id) in strategy_options {
            let is_sel = strategy == option;
            strategy_control = strategy_control.child(self.seg_option(
                id,
                option.label(),
                is_sel,
                cx,
                move |this, cx| apply_profile(this, cx, option),
            ));
        }

        // 偏离提示：只比对本页可见、且套用会覆盖的那几项。用户手改过滑条时先讲清楚，
        // 免得按下去才发现自己的取值被换掉。
        let drifted = self.state.whisper_model_tier != profile.whisper_tier
            || self.state.whisper_threads != profile.whisper_threads
            || self.state.config.pipeline.llm_threads != profile.llm_threads
            || self.state.config.pipeline.parallel_workers != 0
            // GPU 开关也纳入比对：推荐要开而当前关着时，套用会把它打开。
            || (profile.gpu_offload && !self.state.config.gpu.whisper_offload);
        // GPU 转写说明：推荐为真时套用会打开 config.gpu.whisper_offload（下次启动
        // 生效）；推荐为假只代表没检测到独显（核显不算），套用保持当前开关不动。
        let gpu_hint = if profile.gpu_offload {
            "GPU 转写：开（下次启动生效）"
        } else {
            "GPU 转写：保持当前开关（未检测到独显）"
        };
        let summary = format!(
            "{}：{} · {} 线程 · 并发 {} · {}（{}）",
            if drifted {
                "当前已偏离推荐"
            } else {
                "当前即为推荐"
            },
            profile.whisper_model_name,
            profile.whisper_threads,
            profile.max_concurrency,
            gpu_hint,
            strategy.label(),
        );

        let apply_control = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_3))
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(if drifted {
                        Theme::accent_orange()
                    } else {
                        Theme::text_muted()
                    })
                    .child(summary),
            )
            .child(
                primitives::btn_clickable(
                    "套用推荐配置",
                    primitives::BtnSize::Md,
                    primitives::BtnVariant::Primary,
                )
                .id("perf-apply-recommend-btn")
                .on_click(cx.listener(move |this, _, _, cx| {
                    // 按钮与偏好胶囊走同一条链路：按当前偏好重新套用一遍
                    let option = this.state.user_strategy;
                    apply_profile(this, cx, option);
                })),
            )
            .into_any_element();

        primitives::card_rows()
            .child(Self::render_setting_row(
                "硬件评估档位",
                primitives::badge(level.label()).into_any_element(),
            ))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row(
                "用户推理偏好",
                strategy_control.into_any_element(),
            ))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("推荐配置", apply_control))
            .child(
                // 覆盖告知写在明面上：套用会重写下面几张卡里手调的参数
                div()
                    .w_full()
                    .pb(px(Theme::SPACE_2))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child("套用会把推荐档位 / 线程数 / 并行进程数 / VAD 开关写进 config.toml，覆盖你在下方手动调整的取值（对下一次转写生效）；推荐要求开 GPU 转写时会同时打开 config.gpu.whisper_offload（对下一次启动生效，硬件不支持时引擎自动回退纯 CPU）。"),
            )
    }

    /// 配置说明卡：由右上角问号按钮展开，逐条介绍各配置项的作用
    fn render_perf_help_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 「识别引擎」与「Whisper 模型档位」两条已随性能页那张卡一起移除：
        // 引擎与模型现在只在「模型与组件」页和右侧抽屉里选，帮助卡不该再介绍
        // 一个页面上根本没有的配置项——照着找只会找不到。
        let items: [(&'static str, &'static str); 5] = [
            (
                "转写线程数",
                "单个识别进程内的 CPU 线程数。SenseVoice 超过 8 线程后收益递减；Whisper 建议设为物理核心数。",
            ),
            (
                "并行进程数",
                "长音频按时间轴切成多块、每块独立起一个识别进程并发跑。实测多进程远比加线程有效；「自动」按 CPU 核数推导。",
            ),
            (
                "自动标点与语法修正",
                "CT-Punc 毫秒级补标点；Qwen 深度润色更慢，但能纠正错字与口语。",
            ),
            (
                "用户推理偏好",
                "速度优先 / 平衡模式 / 精度优先，一键套用整套推荐配置。",
            ),
            (
                "字幕翻译引擎",
                "本地 Qwen 免费离线；在线 API 支持任意 OpenAI 兼容接口（DeepSeek / OpenAI / 通义等），更快更好但需填密钥。",
            ),
        ];

        div()
            .w_full()
            .p(px(Theme::PAGE_PAD))
            .rounded(px(Theme::CARD_RADIUS))
            .bg(Theme::tint_blue_soft())
            .border_1()
            .border_color(Theme::tint_blue_border())
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(primitives::section_title("配置说明").text_color(Theme::accent_blue()))
                    .child(
                        primitives::btn(
                            "收起",
                            primitives::BtnSize::Xs,
                            primitives::BtnVariant::Secondary,
                        )
                        .id("perf-help-close-btn")
                        .hover(|s| {
                            s.text_color(Theme::text_primary())
                                .border_color(Theme::border_strong())
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.state.show_perf_help = false;
                            cx.notify();
                        })),
                    ),
            )
            .children(items.iter().map(|(name, desc)| {
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(Theme::SPACE_2))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(format!("· {}", name)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_secondary())
                            .child(*desc),
                    )
            }))
    }

    /// 拉取在线翻译服务端的 `/models` 列表（性能页「拉取模型列表」按钮）。
    ///
    /// 请求全程在 `background_executor` 里跑（与 `actions.rs::probe_online_translate_api`
    /// 同一套 `cx.spawn` 写法）：慢网关下绝不能卡住 UI 线程。结果写回
    /// [`MODEL_LIST`] 缓存，渲染用的就是缓存快照。
    fn fetch_translate_models(&mut self, cx: &mut Context<Self>) {
        if model_cache_snapshot(&self.state.config.translate).0 {
            // 已在拉取中：连点不该再发一轮请求
            return;
        }
        let cfg = self.state.config.translate.clone();
        let key = cfg.effective_api_key();
        if cfg.api_base.trim().is_empty() {
            with_model_cache(|slot| {
                *slot = Some(ModelListCache {
                    fingerprint: model_cache_fingerprint(&cfg),
                    loading: false,
                    status: Some((
                        false,
                        "请先填写「接口基址」（例如 http://host:3021/v1）".to_string(),
                    )),
                    models: Vec::new(),
                })
            });
            cx.notify();
            return;
        }
        if key.trim().is_empty() {
            with_model_cache(|slot| {
                *slot = Some(ModelListCache {
                    fingerprint: model_cache_fingerprint(&cfg),
                    loading: false,
                    status: Some((
                        false,
                        "请先填写 API Key（或设置环境变量 VOICE2WORD_API_KEY）".to_string(),
                    )),
                    models: Vec::new(),
                })
            });
            cx.notify();
            return;
        }

        let fingerprint = model_cache_fingerprint(&cfg);
        with_model_cache(|slot| {
            let models = slot
                .as_ref()
                .filter(|c| c.fingerprint == fingerprint)
                .map(|c| c.models.clone())
                .unwrap_or_default();
            *slot = Some(ModelListCache {
                fingerprint: fingerprint.clone(),
                loading: true,
                status: None,
                models,
            });
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { fetch_model_list(&cfg, &key) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(ref outcome) = result {
                    if outcome.url.contains("/v1/models")
                        && !this.state.config.translate.api_base.contains("/v1")
                    {
                        let trimmed = this
                            .state
                            .config
                            .translate
                            .api_base
                            .trim()
                            .trim_end_matches('/');
                        let auto_v1 = format!("{trimmed}/v1");
                        this.state.config.translate.api_base = auto_v1.clone();
                        this.api_base_input = auto_v1;
                        this.state.save_translate_config();
                    }
                }
                with_model_cache(|slot| {
                    *slot = Some(match result {
                        Ok(outcome) => ModelListCache {
                            fingerprint,
                            loading: false,
                            status: Some((
                                true,
                                model_fetch_success_message(
                                    &outcome.url,
                                    outcome.models.len(),
                                    outcome.note.as_deref(),
                                ),
                            )),
                            models: outcome.models,
                        },
                        Err(message) => ModelListCache {
                            fingerprint,
                            loading: false,
                            status: Some((false, message)),
                            models: Vec::new(),
                        },
                    });
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 把下拉里选中的模型写进 `config.translate.api_model` 并落盘。
    ///
    /// `id` 一字不改地写入：本网关要求 `cn:` / `global:` 前缀，任何「顺手裁一下」
    /// 都会换来 400 model_unavailable。同时回填窗口上的编辑缓冲，
    /// 让「模型名」输入框立刻显示新值（否则要点一下输入框才会落旧值回去）。
    fn apply_translate_model(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        self.state.config.translate.api_model = id.to_string();
        self.api_model_input = id.to_string();
        self.state.save_translate_config();
        cx.notify();
    }

    /// 离线链路（本地 Qwen）的模型文件路径行：输入框 + 「浏览…」按钮 + 即时校验提示。
    ///
    /// 需求来源：离线模型此前只能手改 `config.toml`，而「我自己下了一个 GGUF」
    /// 是最自然的诉求。这里给出三条入口——直接在输入框里粘贴路径、点「浏览…」
    /// 用系统对话框挑文件、点「恢复默认」回到内置 Qwen；三条路都汇到
    /// `MainWindow::set_local_model_path`，不存在第二套落盘逻辑。
    ///
    /// 行尾三个可选按钮，都只在有意义时出现：模型缺失时给「下载模型」（下载中换成
    /// 置灰占位），用户改过路径时给「恢复默认」，「浏览…」常驻。
    ///
    /// 状态提示把两路信号合成一条，避免自相矛盾（见函数内注释）：手动编辑的
    /// 校验结果（`local_model_hint`，红字说明哪里不对）优先；没有失败校验时以
    /// 「`qwen-llm` 是否就位」为准，因为「校验通过」只说明文件像个模型，
    /// 不等于引擎真的能用它（例如体积没过 400MB 下限）。
    fn render_local_model_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use crate::ui::ApiField;
        let input = self.render_api_input("local-model-path-input", ApiField::LocalModelPath, cx);
        let browse = primitives::chip_clickable("浏览…", false, false)
            .id("local-model-browse-btn")
            .on_click(cx.listener(|this, _, _, cx| this.choose_llm_model_file(cx)));
        // 「恢复默认」只在用户确实改过路径时出现：与内置默认路径逐字符比对
        // （不做路径归一化——`resolve_path` 只认相对/绝对之分，比对原始字符串
        // 足以判断「是不是用户自己填的」，也不会因为盘符大小写差异误判）。
        // 让用户点一下就回到内置 Qwen，而不是逼他把整条长路径删干净。
        let customized = self.state.config.paths.llm_model.trim()
            != crate::utils::config::PathsConfig::default().llm_model;
        let restore = customized.then(|| {
            primitives::chip_clickable("恢复默认", false, false)
                .id("local-model-restore-btn")
                .on_click(
                    cx.listener(|this, _, _, cx| this.set_local_model_path(String::new(), cx)),
                )
        });
        // 模型没就位时，行尾给一个「下载」直达模型管理页那个条目（引导到「模型与
        // 组件」卡要多滚一屏，这里放个等价按钮更省事）。注意：用户自备的路径若填错，
        // `qwen-llm` 同样会判为未就位，此时点「下载」会按内置默认路径下载——
        // 所以提示文案里同时点了「核对路径」这条出路（见 editor.rs 的翻译卡）。
        let missing_model = !self.state.model_is_present("qwen-llm");
        let downloading = self.state.is_downloading;
        let download = if downloading {
            // 下载中（可能是 qwen-llm 本身，也可能是「一键补齐」的其他条目）：
            // 给一个置灰占位，别让按钮凭空消失、看起来像点错了。
            Some(primitives::chip("下载中…", false, false).id("local-model-downloading"))
        } else if missing_model {
            Some(
                primitives::chip_clickable("下载模型", false, false)
                    .id("local-model-download-btn")
                    .on_click(
                        cx.listener(|this, _, _, cx| this.start_model_download("qwen-llm", cx)),
                    ),
            )
        } else {
            None
        };

        // 两路信号（校验红字 vs. 就位绿字）的合成规则抽在 `local_model_status`，
        // 那里是唯一实现处，也有单测锁住——避免「绿字说路径有效、旁边却挂着
        // 下载按钮」这类自相矛盾回归。
        let (ok, msg) =
            local_model_status(self.local_model_hint.clone(), !missing_model, downloading);

        // 已下载的大模型：每个 `.gguf` 列成一个胶囊，点一下即切换。
        //
        // 为什么要有这一行：输入框 + 「浏览…」要求用户**自己找到并抄下**模型路径，
        // 而路径又长又容易看错（`qwen2.5-1.5b-instruct-q4_k_m.gguf`）。离线模型
        // 多起来之后（0.5B / 1.5B / 3B）这条路尤其难走。这里直接把扫到的文件列出来，
        // 点一下就走同一个 `set_local_model_path`，落盘逻辑不分叉。
        let installed = self.llm_models_snapshot(cx);
        let current_path =
            crate::utils::AppConfig::resolve_path(&self.state.config.paths.llm_model);
        let quick_pick = (!installed.is_empty()).then(|| {
            let mut row = div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(Theme::SPACE_1_5))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(Theme::text_muted())
                        .child("本机已下载："),
                );
            for (i, m) in installed.iter().enumerate() {
                let is_sel = crate::utils::local_models::paths_equal(&m.path, &current_path);
                let target = m.path.clone();
                row = row.child(
                    primitives::chip_clickable(
                        format!(
                            "{}  {}",
                            m.label,
                            crate::utils::model_download::human_size(m.size_bytes)
                        ),
                        is_sel,
                        false,
                    )
                    .id(("local-model-pick", i))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_local_model_path(target.to_string_lossy().to_string(), cx);
                    })),
                );
            }
            row
        });

        div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_1_5))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(input)
                    .child(browse)
                    .children(download)
                    .children(restore),
            )
            .children(quick_pick)
            .children((!ok).then(|| {
                div()
                    .w_full()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::accent_red())
                    .child(msg)
            }))
            .into_any_element()
    }

    /// 模型下拉所在行的「行尾」内容：高亮绿色的 [获取模型列表] 按钮 + 结果状态。
    fn render_model_fetch_row(&self, is_online: bool, cx: &mut Context<Self>) -> AnyElement {
        const MODEL_FETCH_MSG_MAX_W: f32 = 520.0;
        let (loading, status) = model_cache_status(&self.state.config.translate);

        // 1. 高亮绿色的 [获取模型列表] 按钮
        let mut button = div()
            .id("perf-translate-models-btn")
            .px(px(Theme::SPACE_3))
            .py(px(Theme::SPACE_1))
            .rounded(px(Theme::RADIUS_MD))
            .bg(if loading {
                Theme::bg_disabled()
            } else {
                Theme::accent_mint()
            })
            .border_1()
            .border_color(if loading {
                Theme::border()
            } else {
                Theme::accent_mint_deep()
            })
            .text_size(px(Theme::TEXT_SMALL))
            .font_weight(FontWeight::BOLD)
            .text_color(if loading {
                Theme::text_disabled()
            } else {
                Theme::text_on_accent()
            });

        if !loading && is_online {
            button = button
                .cursor_pointer()
                .hover(|s| s.bg(Theme::accent_mint_deep()))
                .on_click(cx.listener(|this, _, _, cx| this.fetch_translate_models(cx)));
        }

        let button_el = button.child(if loading {
            "获取中…"
        } else {
            "[获取模型列表]"
        });

        let mut row = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap(px(Theme::SPACE_2_5))
                    .child(button_el),
            );

        if let Some((false, text)) = status {
            row = row.child(div().flex().flex_col().gap(px(Theme::SPACE_1)).children(
                text.lines().map(move |line| {
                    div()
                        .max_w(px(MODEL_FETCH_MSG_MAX_W))
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::accent_red())
                        .child(line.to_string())
                }),
            ));
        }

        row.into_any_element()
    }

    /// 服务端模型的规整选择区（支持分类Tab过滤、滚动容器防撑屏、纯净ID展示）。
    fn render_model_choice_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let cat = MODEL_CATEGORY.lock().map(|g| g.clone()).unwrap_or_default();
        let selected = self.state.config.translate.api_model.trim().to_string();
        // 借用式读取：只需要 id、分类计数与总数，不再逐帧深拷整份模型表
        with_fresh_model_cache(&self.state.config.translate, |cache| match cache {
            Some(cache) if !cache.loading && !cache.models.is_empty() => {
                self.render_model_pool(&cache.models, &cat, &selected, cx)
            }
            _ => div().into_any_element(),
        })
    }

    /// 「可用模型池」的实际内容（见 [`Self::render_model_choice_row`]）。
    ///
    /// 模型表 / 分类 / 当前选中 id 全部按借用传入，渲染期不复制模型表。
    fn render_model_pool(
        &self,
        models: &[ModelInfo],
        cat: &str,
        selected: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cn_count = models.iter().filter(|m| m.id.starts_with("cn:")).count();
        let global_count = models
            .iter()
            .filter(|m| m.id.starts_with("global:"))
            .count();

        let filtered_models: Vec<&ModelInfo> = models
            .iter()
            .filter(|m| match cat {
                "cn" => m.id.starts_with("cn:"),
                "global" => m.id.starts_with("global:"),
                _ => true,
            })
            .collect();

        let mut container = div()
            .w_full()
            .p(px(Theme::SPACE_2_5))
            .rounded(px(Theme::RADIUS_MD))
            .bg(Theme::bg_track())
            .border_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_2));

        // 顶部操作栏：标题 + 当前选定提示 + 分类过滤 Tabs
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(format!("可用模型池（{} 个）", models.len())),
                    )
                    .child(if !selected.is_empty() {
                        div()
                            .px(px(Theme::SPACE_1_5))
                            .py(px(Theme::SPACE_0_5))
                            .rounded(px(Theme::RADIUS_SM))
                            .bg(Theme::tint_mint_soft())
                            .border_1()
                            .border_color(Theme::tint_mint_border())
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::accent_mint())
                            .child(format!("当前：{selected}"))
                    } else {
                        div()
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_1))
                    .child(
                        primitives::chip(
                            format!("全部（{}）", models.len()),
                            cat.is_empty(),
                            false,
                        )
                        .id("model-cat-all")
                        .cursor_pointer()
                        .on_click(cx.listener(|_, _, _, cx| {
                            if let Ok(mut g) = MODEL_CATEGORY.lock() {
                                *g = String::new();
                            }
                            cx.notify();
                        })),
                    )
                    .when(cn_count > 0, |d| {
                        d.child(
                            primitives::chip(format!("国内（{cn_count}）"), cat == "cn", false)
                                .id("model-cat-cn")
                                .cursor_pointer()
                                .on_click(cx.listener(|_, _, _, cx| {
                                    if let Ok(mut g) = MODEL_CATEGORY.lock() {
                                        *g = "cn".to_string();
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .when(global_count > 0, |d| {
                        d.child(
                            primitives::chip(
                                format!("国际（{global_count}）"),
                                cat == "global",
                                false,
                            )
                            .id("model-cat-global")
                            .cursor_pointer()
                            .on_click(cx.listener(|_, _, _, cx| {
                                if let Ok(mut g) = MODEL_CATEGORY.lock() {
                                    *g = "global".to_string();
                                }
                                cx.notify();
                            })),
                        )
                    }),
            );
        container = container.child(header);

        // 核心滚动网格：限制高度 max-h 140px，带平滑滚动，绝不霸占整个屏幕
        //
        // 滚轮判据与批量队列清单同源（见 `primitives::should_consume_scroll`）：模型池
        // 是**嵌在可滚动设置页里**的小网格，而 GPUI 的内建滚动监听不会阻止冒泡——
        // 少了这一刀，鼠标停在模型池上滚轮会「网格和整页一起滚」，用户根本对不准列表。
        let pool_scroll =
            cx.update_default_global::<ModelPoolScroll, _>(|state, _| state.handle.clone());
        let mut scroll_box = div()
            .id("perf-model-scroll-box")
            .w_full()
            .max_h(px(140.0))
            .overflow_y_scroll()
            .track_scroll(&pool_scroll)
            .on_scroll_wheel(cx.listener(|_, event: &ScrollWheelEvent, _window, cx| {
                let delta_y = match event.delta {
                    ScrollDelta::Lines(p) => p.y,
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                };
                let consume = cx.update_default_global::<ModelPoolScroll, _>(|state, _| {
                    let offset_y = f32::from(state.handle.offset().y);
                    let max_h = f32::from(state.handle.max_offset().height);
                    crate::ui::primitives::should_consume_scroll(delta_y, offset_y, max_h)
                });
                if consume {
                    cx.stop_propagation();
                }
            }))
            .flex()
            .flex_wrap()
            .gap(px(Theme::SPACE_1_5))
            .p(px(Theme::SPACE_1));

        for model in filtered_models {
            let id = model.id.clone();
            let is_sel = id == selected;
            scroll_box = scroll_box.child(
                primitives::chip(id.clone(), is_sel, false)
                    .id(SharedString::from(format!("perf-fetched-model-{id}")))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.apply_translate_model(&id, cx);
                    })),
            );
        }

        container.child(scroll_box).into_any_element()
    }

    /// 设置行通用布局：左侧名称，右侧单行等级选择器（简约，无描述小字）
    fn render_setting_row(label: &'static str, control: AnyElement) -> Div {
        primitives::setting_row(label, control)
    }

    /// 设置行之间的细分隔线
    fn render_setting_divider() -> Div {
        primitives::divider()
    }

    /// 单行等级选择胶囊：紧凑分段控件的最小单元。
    /// 外观全部来自 [`primitives::segmented`]，这里只补交互。
    fn seg_option(
        &self,
        id: &'static str,
        label: &'static str,
        is_sel: bool,
        cx: &mut Context<Self>,
        on_select: impl Fn(&mut Self, &mut Context<Self>) + Copy + 'static,
    ) -> Stateful<Div> {
        primitives::segmented(label, is_sel, false)
            .id(id)
            .on_click(cx.listener(move |this, _, _, cx| on_select(this, cx)))
    }

    /// 步骤 4.5：音频前端预处理与救场解码。
    ///
    /// # 为什么这一卡必须存在
    ///
    /// 这些开关（降噪 / 归一 / 高通 / 停顿压实 / 音频加速 / VAD 阈值 / 低置信救场）
    /// 全部**早已接线进管线**，`README` 也把它们写成卖点（「弱语音场景下提升识别率」），
    /// 但此前没有任何界面入口——只有翻开 `config.toml` 才知道它们存在。功能做了却
    /// 没人能开，等于没做；而弱语音素材正是这些开关唯一有价值的场景。
    ///
    /// # 为什么默认收起
    ///
    /// 默认值已是实测最优（增强全开、压实关闭、救场关闭），误改会**变慢或变差**。
    /// 用「展开」把入口藏一层，既让需要的人找得到，又不让默认路径上的用户碰到。
    fn render_audio_pipeline_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.state.show_audio_advanced;
        let p = &self.state.config.pipeline;
        let enabled = p.preprocess_enabled;
        let denoise = p.preprocess_denoise;
        let normalize = p.preprocess_normalize;
        let compact = p.preprocess_compact;
        let highpass = p.preprocess_highpass_hz;
        let speed = p.whisper_audio_speed;
        let vad = p.whisper_vad_threshold;
        let rescue = p.whisper_rescue_logprob;

        // 总开关：关掉即完全回到「原始音频直喂模型」的旧行为（便于 A/B 对比）。
        let master = primitives::pill_btn_solid(
            if enabled { "已开启" } else { "已关闭" },
            if enabled {
                Theme::accent_mint()
            } else {
                Theme::bg_card_hover()
            },
        )
        .id("perf-audio-preprocess-toggle")
        .text_color(if enabled {
            Theme::text_on_accent()
        } else {
            Theme::text_muted()
        })
        .on_click(cx.listener(|this, _, _, cx| {
            let next = !this.state.config.pipeline.preprocess_enabled;
            this.state.config.pipeline.preprocess_enabled = next;
            this.state.persist_config();
            cx.notify();
        }))
        .into_any_element();

        let mut card = primitives::card_rows().child(
            div()
                .w_full()
                .py(px(Theme::SPACE_2))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(Theme::SPACE_3))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(Theme::SPACE_0_5))
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_BODY_LG))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_primary())
                                .child("音频预处理与救场"),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(Theme::text_muted())
                                .child("默认已是最优组合；弱语音 / 远场素材可在此微调"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(Theme::SPACE_2))
                        .child(master)
                        .child(
                            primitives::mini_btn(if open { "收起" } else { "展开" }, true)
                                .id("perf-audio-advanced-toggle")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.show_audio_advanced =
                                        !this.state.show_audio_advanced;
                                    cx.notify();
                                })),
                        ),
                ),
        );

        if !open {
            return card;
        }

        // 三个增强开关：走同一个小胶囊工厂，选中态用薄荷色（与全局导航一致）。
        let toggle = |id: &'static str,
                      label: &'static str,
                      on: bool,
                      cx: &mut Context<Self>,
                      set: fn(&mut crate::utils::config::PipelineConfig, bool)|
         -> Stateful<Div> {
            primitives::segmented(label, on, false)
                .id(id)
                .on_click(cx.listener(move |this, _, _, cx| {
                    set(&mut this.state.config.pipeline, !on);
                    this.state.persist_config();
                    cx.notify();
                }))
        };

        let enhancements = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(toggle(
                "perf-pre-denoise",
                "谱减降噪",
                denoise,
                cx,
                |c, v| c.preprocess_denoise = v,
            ))
            .child(toggle(
                "perf-pre-normalize",
                "电平归一",
                normalize,
                cx,
                |c, v| c.preprocess_normalize = v,
            ))
            .child(toggle(
                "perf-pre-compact",
                "停顿压实",
                compact,
                cx,
                |c, v| c.preprocess_compact = v,
            ))
            .into_any_element();

        // 三个滑条用整数档位（渲染器只吃 u32），显示时再换算回物理量。
        let hp_val = highpass.clamp(0.0, 200.0).round() as u32;
        let hp_slider = self.render_slider(
            "perf-slider-highpass",
            "高通截止",
            hp_val,
            0,
            200,
            if hp_val == 0 {
                "关闭".to_string()
            } else {
                format!("{hp_val} Hz")
            },
            String::new(),
            cx,
            move |this, v, cx| {
                this.state.config.pipeline.preprocess_highpass_hz = v as f64;
                this.state.persist_config();
                cx.notify();
            },
        );

        let speed_val = (speed.clamp(1.0, 1.5) * 100.0).round() as u32;
        let speed_slider = self.render_slider(
            "perf-slider-audio-speed",
            "输入音频加速",
            speed_val,
            100,
            150,
            format!("{:.2}×", speed_val as f64 / 100.0),
            String::new(),
            cx,
            move |this, v, cx| {
                this.state.config.pipeline.whisper_audio_speed = v as f64 / 100.0;
                this.state.persist_config();
                cx.notify();
            },
        );

        // 量程由引擎的有效区间推导，不写死：滑条能选到的值必须全部真的生效
        // （引擎 `set_vad_threshold` 夹到同一区间，否则「拖到 0.15 实际跑 0.30」）。
        let vad_val = vad_slider_value(vad);
        let vad_slider = self.render_slider(
            "perf-slider-vad",
            "静音检测阈值",
            vad_val,
            (crate::engines::whisper::VAD_THRESHOLD_MIN * 100.0).round() as u32,
            (crate::engines::whisper::VAD_THRESHOLD_MAX * 100.0).round() as u32,
            format!("{:.2}", vad_val as f64 / 100.0),
            String::new(),
            cx,
            move |this, v, cx| {
                this.state.config.pipeline.whisper_vad_threshold = v as f64 / 100.0;
                this.state.persist_config();
                cx.notify();
            },
        );

        let rescue_on = rescue < 0.0;
        let rescue_control = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(if rescue_on {
                        format!("阈值 {rescue:.2}")
                    } else {
                        "关闭".to_string()
                    }),
            )
            .child(
                primitives::segmented("低置信二段重解码", rescue_on, false)
                    .id("perf-rescue-toggle")
                    .on_click(cx.listener(|this, _, _, cx| {
                        // 关闭写回 0.0（与 `default_rescue_logprob` 同值）；
                        // 开启给一个实测可用的保守值 -0.65。
                        let cur = this.state.config.pipeline.whisper_rescue_logprob;
                        this.state.config.pipeline.whisper_rescue_logprob =
                            if cur < 0.0 { 0.0 } else { -0.65 };
                        this.state.persist_config();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child("对低置信窗口重新解码一遍（更准但明显更慢，长片慎用）"),
            )
            .into_any_element();

        // 总开关关闭时把增强项整块调暗：仍可见可改（便于提前配好），但明确未生效。
        let dim = move |el: AnyElement| -> AnyElement {
            if enabled {
                el
            } else {
                div().opacity(0.45).child(el).into_any_element()
            }
        };

        card = card
            .child(Self::render_setting_divider())
            .child(
                div()
                    .w_full()
                    .py(px(Theme::SPACE_2))
                    .child(dim(enhancements)),
            )
            .child(Self::render_setting_divider())
            .child(hp_slider)
            .child(Self::render_setting_divider())
            .child(speed_slider)
            .child(Self::render_setting_divider())
            .child(vad_slider)
            .child(Self::render_setting_divider())
            .child(
                div()
                    .w_full()
                    .py(px(Theme::SPACE_2))
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_1_5))
                    .child(rescue_control),
            );

        // 返回 `card`：上面是赋值语句（值为 `()`），必须以表达式收尾才能满足
        // `impl IntoElement` 的返回类型。
        card
    }

    /// 步骤 3：并行度设置（转写线程数 / 并行进程数 / 润色线程数）
    ///
    /// 三项都用滑条，量程按本机核心数推导：线程数上限 = 逻辑核心数；
    /// 进程数上限按当前识别引擎区分：SenseVoice = MAX_SV_WORKERS(6)、
    /// Whisper = MAX_WORKERS(4)；最左档为「自动」，即按核数推导。
    /// 改动即时写入 config.toml 并作用于下一次任务。
    fn render_parallelism_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cores = crate::app::AppState::logical_cores();

        // 量程按当前所选识别引擎取（见 `parallel_process_max` / `thread_slider_max`）。
        let is_sv = self.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;
        let thread_min = thread_slider_min(is_sv);
        let thread_max = thread_slider_max(is_sv, cores);
        let thread_val = self.state.whisper_threads.clamp(thread_min, thread_max);
        // SenseVoice 单会话线程封顶 8：在数值旁显式标注，避免「选到 16 却没变快」的困惑。
        let thread_text = if is_sv {
            format!(
                "{thread_val} 线程（SenseVoice 上限 {}）",
                crate::engines::sensevoice::MAX_THREADS
            )
        } else {
            format!("{thread_val} 线程")
        };

        let auto_procs = crate::engines::sensevoice_worker_count(cores) as u32;
        let proc_max = parallel_process_max(is_sv);
        let proc_val = self.state.config.pipeline.parallel_workers.min(proc_max);
        let proc_text = if proc_val == 0 {
            if is_sv {
                format!("自动（{auto_procs}）")
            } else {
                "自动".to_string()
            }
        } else {
            format!("{proc_val} 进程")
        };

        let thread_slider = self.render_slider(
            "perf-slider-threads",
            "转写线程数",
            thread_val,
            thread_min,
            thread_max,
            thread_text,
            String::new(),
            cx,
            move |this, v, cx| {
                if this.state.whisper_threads != v {
                    this.state.whisper_threads = v;
                    this.state.config.pipeline.whisper_threads = v;
                    this.state.persist_config();
                    cx.notify();
                }
            },
        );

        let proc_slider = self.render_slider(
            "perf-slider-processes",
            "并行进程数",
            proc_val,
            0,
            proc_max,
            proc_text,
            String::new(),
            cx,
            move |this, v, cx| {
                if this.state.config.pipeline.parallel_workers != v {
                    this.state.config.pipeline.parallel_workers = v;
                    this.state.pipeline.set_parallel_workers(v as usize);
                    this.state.persist_config();
                    cx.notify();
                }
            },
        );

        let llm_val = self
            .state
            .config
            .pipeline
            .llm_threads
            .clamp(1, cores.max(1));
        let llm_slider = self.render_slider(
            "perf-slider-llm-threads",
            "润色线程数",
            llm_val,
            1,
            cores.max(1),
            format!("{llm_val} 线程"),
            String::new(),
            cx,
            move |this, v, cx| {
                if this.state.config.pipeline.llm_threads != v {
                    this.state.config.pipeline.llm_threads = v;
                    this.state.pipeline.set_llm_threads(v);
                    this.state.persist_config();
                    cx.notify();
                }
            },
        );

        primitives::card_rows()
            .child(thread_slider)
            .child(Self::render_setting_divider())
            .child(proc_slider)
            .child(Self::render_setting_divider())
            .child(llm_slider)
    }

    /// 通用滑条：上行「标签 + 当前值」，中间为可点击 / 可拖动的轨道，下方为提示文案。
    ///
    /// GPUI 0.2 没有内置滑条，这里用「每档一个隐形命中格」实现：
    /// 视觉层（底轨 / 填充 / 手柄）绝对定位，命中层是 `max - min + 1` 个等宽透明格子，
    /// 各自在按下与按住移动时把本档取值回传。这样无需任何坐标换算，
    /// 也不依赖窗口尺寸，天然适配不同 DPI 与布局。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_slider(
        &self,
        id: &'static str,
        label: &'static str,
        value: u32,
        min: u32,
        max: u32,
        value_text: String,
        hint: String,
        cx: &mut Context<Self>,
        on_change: impl Fn(&mut Self, u32, &mut Context<Self>) + Copy + 'static,
    ) -> Stateful<Div> {
        let max = max.max(min);
        let span = (max - min).max(1) as f32;
        let ratio = ((value.clamp(min, max) - min) as f32 / span).clamp(0.0, 1.0);

        div()
            .id(id)
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_4))
            .py(px(Theme::SPACE_2_5))
            // 拖动落盘闸门：轨道上的按下/移动只更新内存（实时预览照常），松手
            // ——含把指针拖出行外才松手——在这里统一落盘一次。否则一次拖动会在
            // UI 线程上打出几十上百次同步写，既卡顿又刷盘。
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.state.end_config_drag();
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.state.end_config_drag();
                    cx.notify();
                }),
            )
            // 左侧：标签 + 提示说明
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_1))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY_LG))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(label),
                    )
                    .when(!hint.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .text_color(Theme::text_muted())
                                .child(hint),
                        )
                    }),
            )
            // 右侧：紧凑高级步进控制组 (定宽 220px，拒绝通栏傻长大滑轨)
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(220.0))
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_2))
                    .items_end()
                    // 1) 步进微调：[-] [ 当前值徽章 ] [+]
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                primitives::chip_clickable("－", false, value <= min)
                                    .id(SharedString::from(format!("{id}-dec")))
                                    .when(value > min, |d| {
                                        let next = value.saturating_sub(1).max(min);
                                        d.on_click(cx.listener(move |this, _, _, cx| {
                                            on_change(this, next, cx)
                                        }))
                                    }),
                            )
                            .child(primitives::badge_accent(value_text))
                            .child(
                                primitives::chip_clickable("＋", false, value >= max)
                                    .id(SharedString::from(format!("{id}-inc")))
                                    .when(value < max, |d| {
                                        let next = value.saturating_add(1).min(max);
                                        d.on_click(cx.listener(move |this, _, _, cx| {
                                            on_change(this, next, cx)
                                        }))
                                    }),
                            ),
                    )
                    // 2) 定长精致微滑轨 (高 6px，薄荷绿胶囊指示)
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(Theme::SPACE_4))
                            .flex()
                            .items_center()
                            // 底轨 + 已选填充
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .right_0()
                                    .h(px(Theme::PROGRESS_H))
                                    .rounded_full()
                                    .bg(Theme::bg_track())
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(ratio))
                                            .rounded_full()
                                            .bg(Theme::accent_mint()),
                                    ),
                            )
                            // 手柄：外圈描边，内圈薄荷色
                            .child(
                                div()
                                    .absolute()
                                    .left(relative(ratio))
                                    .ml(px(-7.0))
                                    .w(px(14.0))
                                    .h(px(14.0))
                                    .rounded_full()
                                    .bg(Theme::bg_card())
                                    .border_1()
                                    .border_color(Theme::border())
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .w(px(6.0))
                                            .h(px(6.0))
                                            .rounded_full()
                                            .bg(Theme::accent_mint()),
                                    ),
                            )
                            // 命中层：每档一个隐形格子，支持单击与按住拖动
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .right_0()
                                    .top_0()
                                    .bottom_0()
                                    .flex()
                                    .cursor_pointer()
                                    .children((min..=max).map(|v| {
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .h_full()
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, _, cx| {
                                                    this.state.begin_config_drag();
                                                    on_change(this, v, cx)
                                                }),
                                            )
                                            .on_mouse_move(cx.listener(
                                                move |this, event: &MouseMoveEvent, _, cx| {
                                                    if event.pressed_button
                                                        == Some(MouseButton::Left)
                                                    {
                                                        this.state.begin_config_drag();
                                                        on_change(this, v, cx);
                                                    }
                                                },
                                            ))
                                    })),
                            ),
                    ),
            )
    }

    /// 步骤 4：标点恢复与 AI 润色引擎 (每行一个配置项，右侧单行等级选择)
    fn render_polish_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enable_polish = self.state.enable_polish;
        let is_punc = self.state.polish_mode == crate::app::PolishMode::PuncFast;

        let toggle_btn = primitives::pill_btn_solid(
            if enable_polish {
                "功能已开启"
            } else {
                "功能已关闭"
            },
            if enable_polish {
                Theme::accent_mint()
            } else {
                Theme::bg_card_hover()
            },
        )
        .id("perf-toggle-polish-btn")
        .text_color(if enable_polish {
            Theme::text_on_accent()
        } else {
            Theme::text_muted()
        })
        .on_click(cx.listener(|this, _, _, cx| {
            // 走 setter 落盘：只翻内存里的布尔量会让「关掉润色」在重启后失效
            // （配置里仍是 true，启动又读回来）。模式沿用当前档位。
            let next = !this.state.enable_polish;
            let mode = this.state.polish_mode;
            this.state.set_polish(next, mode);
            cx.notify();
        }))
        .into_any_element();

        let mode_control = primitives::segmented_cluster()
            .child(self.seg_option(
                "perf-select-punc-fast",
                "CT-Punc 极速标点",
                is_punc && enable_polish,
                cx,
                |this, cx| {
                    // 点选润色引擎 = 明确要润色，顺手打开总开关（并落盘）
                    this.state
                        .set_polish(true, crate::app::PolishMode::PuncFast);
                    cx.notify();
                },
            ))
            .child(self.seg_option(
                "perf-select-qwen-deep",
                "Qwen 深度润色",
                !is_punc && enable_polish,
                cx,
                |this, cx| {
                    this.state
                        .set_polish(true, crate::app::PolishMode::QwenDeep);
                    cx.notify();
                },
            ))
            .into_any_element();
        // 总开关关闭时降透明度提示「未生效」，但行保持可见可点
        let mode_control = if enable_polish {
            mode_control
        } else {
            div().opacity(0.45).child(mode_control).into_any_element()
        };

        primitives::card_rows()
            .child(Self::render_setting_row("自动标点与语法修正", toggle_btn))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("润色引擎", mode_control))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        effective_glossary_count, glossary_count_cached, local_model_status, models_snapshot_stale,
        probe_button_label, vad_slider_value, ModelsKey,
    };
    use crate::utils::config::TranslateConfig;

    /// 「测试连接」按钮的两态文案：探测中必须换成进行时，否则用户会以为还能再点一次
    /// （连点会连发多轮网络请求），也看不出上一轮请求还没回来。
    #[test]
    fn probe_button_label_switches_while_probing() {
        assert_eq!(probe_button_label(false), "测试连接");
        assert_eq!(probe_button_label(true), "测试中…");
        assert!(
            !probe_button_label(true).starts_with("测试连接"),
            "探测中不能再显示可点状态的文案"
        );
        assert!(
            probe_button_label(true).ends_with('…'),
            "进行态要用省略号，让用户知道请求还在路上"
        );
    }

    /// 回归（P2）：术语表计数必须与**实际注入**一致——超过上限时显示生效数，
    /// 而不是解析出的全部条数。
    #[test]
    fn glossary_count_caps_at_injection_limit() {
        let cap = TranslateConfig::MAX_GLOSSARY_ENTRIES;
        assert_eq!(effective_glossary_count(0, cap), 0);
        assert_eq!(effective_glossary_count(3, cap), 3);
        assert_eq!(effective_glossary_count(cap, cap), cap);
        assert_eq!(effective_glossary_count(cap + 40, cap), cap);
        // 上限放宽后不该再截断（配置项生效的证据）
        assert_eq!(effective_glossary_count(cap + 40, cap + 100), cap + 40);
    }

    /// 术语表条数缓存的失效判据：同一份原文命中缓存，原文一改必须重算
    /// （否则用户改完术语表，界面上的条数会停在旧值）。
    #[test]
    fn glossary_count_cache_follows_content_and_reuses_within_same_content() {
        let two_entries = TranslateConfig {
            glossary: "a=b\nc=d\n".to_string(),
            ..TranslateConfig::default()
        };
        // 与上一份**字节等长**但只有一条合法条目：只按长度做键的实现会在这里返回 2
        let one_entry = TranslateConfig {
            glossary: "aa=bcd=\n".to_string(),
            ..TranslateConfig::default()
        };
        assert_eq!(
            two_entries.glossary.len(),
            one_entry.glossary.len(),
            "用例前提：两份原文长度相同"
        );

        assert_eq!(glossary_count_cached(&two_entries), 2);
        assert_eq!(glossary_count_cached(&one_entry), 1, "原文变了必须重算");
        assert_eq!(glossary_count_cached(&two_entries), 2, "换回原文也要重算");
        assert_eq!(glossary_count_cached(&two_entries), 2, "同内容复用缓存");
    }

    /// 回归：VAD 滑条量程必须落在引擎的有效区间内。
    ///
    /// 此前滑条写死 `(10, 90)` 而引擎夹到 `(0.3, 0.8)`：用户拖到 0.15 时界面显示
    /// 0.15、config.toml 也存了 0.15，引擎却按 0.30 跑——「改了没效果」且无从察觉。
    /// 量程现在由 `whisper::VAD_THRESHOLD_{MIN,MAX}` 推导，这条断言把两者钉在一起。
    #[test]
    fn vad_slider_range_matches_engine_clamp() {
        use crate::engines::whisper::{VAD_THRESHOLD_MAX, VAD_THRESHOLD_MIN};
        let slider_min = (VAD_THRESHOLD_MIN * 100.0).round() as u32;
        let slider_max = (VAD_THRESHOLD_MAX * 100.0).round() as u32;
        // 滑条的每一个取值换算回物理量后都必须**原样**通过引擎夹取
        for v in slider_min..=slider_max {
            let as_f64 = v as f64 / 100.0;
            let clamped = as_f64.clamp(VAD_THRESHOLD_MIN, VAD_THRESHOLD_MAX);
            assert_eq!(
                as_f64, clamped,
                "滑条档位 {v}（{as_f64}）会被引擎悄悄改成 {clamped}"
            );
        }
        // 默认值必须落在量程内，否则一进页面就显示被夹过的值
        let cfg = crate::utils::config::AppConfig::default();
        let d = cfg.pipeline.whisper_vad_threshold;
        assert!(
            (VAD_THRESHOLD_MIN..=VAD_THRESHOLD_MAX).contains(&d),
            "默认 VAD 阈值 {d} 落在有效区间外"
        );
    }

    /// 回归：并行进程数滑条量程必须与**该引擎实际夹取的上限**一致。
    ///
    /// 此前量程无条件取 `MAX_WORKERS`(4)：默认引擎是 SenseVoice（上限 6），
    /// 界面上 5/6 两档直接消失，而引擎本可跑满 6 —— 静默的能力削减。
    /// 更早的版本则是量程取自动值（16 核下 6）而不分引擎，Whisper 侧显示 5/6、
    /// 实际却被夹到 4。这条断言把「量程 == 引擎夹取值」钉死在两端。
    #[test]
    fn parallel_process_slider_max_matches_engine_clamp() {
        use super::parallel_process_max;
        use crate::app::WhisperModelTier;
        use crate::engines::chunked_whisper::{MAX_SV_WORKERS, MAX_WORKERS};

        let sv_max = parallel_process_max(true) as usize;
        let whisper_max = parallel_process_max(false) as usize;
        assert_eq!(sv_max, MAX_SV_WORKERS, "SenseVoice 量程应放开到 6");
        assert_eq!(whisper_max, MAX_WORKERS, "Whisper 量程不得超过 4");
        assert!(sv_max > whisper_max, "SenseVoice 必须比 Whisper 更宽");

        // 滑条上的每一档都必须是引擎会**原样接受**的值（不被静默夹小）
        for w in 1..=sv_max {
            assert_eq!(
                w.clamp(1, MAX_SV_WORKERS),
                w,
                "SenseVoice 滑条档位 {w} 会被引擎悄悄改成更小的值"
            );
        }
        for w in 1..=whisper_max {
            assert_eq!(
                w.clamp(1, MAX_WORKERS),
                w,
                "Whisper 滑条档位 {w} 会被引擎悄悄改成更小的值"
            );
        }

        // 自动档（0）不受量程约束，且两条路径的自动值都落在各自量程内
        let cores = crate::app::AppState::logical_cores();
        assert!(
            crate::engines::sensevoice_worker_count(cores) <= sv_max,
            "自动进程数越出 SenseVoice 量程"
        );
        assert!(
            crate::engines::chunked_whisper::worker_count(64, cores.max(8), false) <= whisper_max,
            "自动进程数越出 Whisper 量程"
        );
        // 档位类型与量程取值的对应关系不能被改反
        assert!(matches!(
            WhisperModelTier::SenseVoice.model_relative_path().to_ascii_lowercase(),
            ref m if m.ends_with(".onnx")
        ));
    }
    /// 回归：转写线程数滑条量程必须与**该引擎实际夹取的线程上限**一致。
    ///
    /// SenseVoice 单会话线程在引擎里夹到 `MAX_THREADS`(8)，此前滑条量程一律取逻辑
    /// 核数（16 核 → 16）：用户选 9~16 时界面与 config.toml 都记着 16，引擎却按 8
    /// 跑。Whisper 侧无此上限，量程应保持在逻辑核数。
    #[test]
    fn thread_slider_max_matches_engine_clamp() {
        use super::{thread_slider_max, thread_slider_min};
        use crate::engines::sensevoice::{MAX_THREADS, MIN_THREADS};

        // 常量不等式放进 `const` 块，规避 clippy 的 `assertions_on_constants`。
        const { assert!(MIN_THREADS <= MAX_THREADS) };

        assert_eq!(
            thread_slider_min(true),
            MIN_THREADS,
            "SenseVoice 最左档取引擎下限"
        );
        assert_eq!(thread_slider_min(false), 2, "Whisper 最左档保持 2");

        for cores in [1u32, 2, 4, 8, 16, 32, 64] {
            let sv_max = thread_slider_max(true, cores);
            assert!(
                (MIN_THREADS..=MAX_THREADS).contains(&sv_max),
                "SenseVoice 量程 {sv_max}（{cores} 核）越出 [{MIN_THREADS}, {MAX_THREADS}]"
            );
            // 滑条每一档都必须被引擎原样接受（不被静默夹小）
            for v in thread_slider_min(true)..=sv_max {
                assert_eq!(
                    v.clamp(MIN_THREADS, MAX_THREADS),
                    v,
                    "SenseVoice 滑条档位 {v}（{cores} 核）会被引擎悄悄改成更小的值"
                );
            }
            // Whisper 侧无单会话上限：量程 == 逻辑核数（至少 2），且引擎原样接受
            assert_eq!(
                thread_slider_max(false, cores),
                cores.max(2),
                "Whisper 线程量程应保持为逻辑核数"
            );
        }

        // 16 核是「滑条给到 16、引擎夹回 8」的原发场景
        assert_eq!(thread_slider_max(true, 16), MAX_THREADS);
        assert_eq!(thread_slider_max(true, 4), 4);
        assert_eq!(thread_slider_max(false, 16), 16);

        // 设置页实际显示的线程值必须落在引擎有效区间内
        let cores = crate::app::AppState::logical_cores();
        let shown = crate::utils::config::AppConfig::default()
            .pipeline
            .whisper_threads
            .clamp(thread_slider_min(true), thread_slider_max(true, cores));
        assert!(
            (MIN_THREADS..=MAX_THREADS).contains(&shown),
            "SenseVoice 页面显示线程数 {shown} 落在有效区间外"
        );
    }

    /// 回归：离线模型行「校验结果」与「是否就位」两路信号必须合成一条不矛盾的提示。
    /// 最容易写错的两处：① 校验通过但模型其实没就位（例如自备 GGUF 体积没过
    /// 400MB 下限）时不该显示成「放心了」；② 按过「恢复默认」后内置模型还没下，
    /// 也不该给确认。两者都必须落到「未就位，下一步怎么办」。
    #[test]
    fn local_model_status_never_claims_ready_when_model_missing() {
        // 编辑过且校验失败：红字原因原样透传，优先于就位判定
        let (ok, msg) = local_model_status(
            Some((
                false,
                "找不到模型文件：D:\\x.gguf（相对路径以项目根为基准）".to_string(),
            )),
            false,
            false,
        );
        assert!(!ok);
        assert!(msg.contains("找不到模型文件"), "应透传校验原因: {msg}");

        // 编辑过、校验通过，但 qwen-llm 仍未就位（体积没过下限）：
        // 不能报「路径有效」，要说未就位 + 下一步动作
        let (ok, msg) = local_model_status(Some((true, "路径有效".to_string())), false, false);
        assert!(!ok, "校验通过 ≠ 引擎可用，模型未就位时不得中性确认");
        assert!(msg.contains("未就位"), "应提示未就位: {msg}");

        // 编辑过、校验通过、模型确实就位：中性确认
        let (ok, msg) = local_model_status(Some((true, "路径有效".to_string())), true, false);
        assert!(ok);
        assert_eq!(msg, "路径有效");

        // 没编辑过、模型就位：中性说明（不报红）
        let (ok, msg) = local_model_status(None, true, false);
        assert!(ok);
        assert!(msg.contains("已就位"), "{msg}");

        // 没编辑过、模型未就位：红字 + 可操作指引
        let (ok, msg) = local_model_status(None, false, false);
        assert!(!ok);
        assert!(msg.contains("下载模型"), "{msg}");

        // 下载中：没有失败校验时给中性「正在下载…」（别让「未就位」看起来像没反应）
        let (ok, msg) = local_model_status(None, false, true);
        assert!(ok);
        assert!(msg.contains("正在下载"), "{msg}");

        // 但**失败校验优先级最高**，下载中也不盖掉：下载的是内置默认模型，
        // 而配置里的路径仍是错的，下完照样用不了——红字与「恢复默认」必须同时在场。
        let (ok, msg) =
            local_model_status(Some((false, "找不到模型文件".to_string())), false, true);
        assert!(!ok, "路径错误必须持续可见，下载遮不住它");
        assert!(msg.contains("找不到模型文件"), "{msg}");
    }

    /// 回归（P2）：VAD 配置值越界时，滑条档位必须先夹后取整——
    /// 与引擎 `set_vad_threshold` 的夹取区间对齐：0.15 这类旧配置
    /// 界面上显示的档位要和引擎实际生效的区间一致。
    #[test]
    fn vad_slider_value_survives_out_of_range_config() {
        // 低于下限：钉在 30 档（0.30）
        assert_eq!(vad_slider_value(0.05), 30);
        assert_eq!(vad_slider_value(0.1), 30);
        // 高于上限：钉在 80 档（0.80）
        assert_eq!(vad_slider_value(0.95), 80);
        // 区间内：原样换算
        assert_eq!(vad_slider_value(0.5), 50);
    }

    /// 模型快照的失效判定：键不变 → 复用（这是「渲染路径不 read_dir」的前提）。
    #[test]
    fn models_snapshot_reuses_when_key_unchanged() {
        let k = ModelsKey {
            downloading: false,
            dir_mtime: Some((100, 0)),
        };
        assert!(!models_snapshot_stale(Some(&k), &k), "键不变时不该重扫");
    }

    /// 无缓存（首次渲染）→ 必须重扫一次。
    #[test]
    fn models_snapshot_scans_when_never_computed() {
        let k = ModelsKey {
            downloading: false,
            dir_mtime: None,
        };
        assert!(models_snapshot_stale(None, &k), "首次必须扫一次");
    }

    /// 下载完成（downloading true→false）→ 键变化 → 重扫，菜单自动刷新。
    #[test]
    fn models_snapshot_rescans_after_download_finishes() {
        let cached = ModelsKey {
            downloading: true,
            dir_mtime: Some((100, 0)),
        };
        let current = ModelsKey {
            downloading: false,
            dir_mtime: Some((100, 0)),
        };
        assert!(
            models_snapshot_stale(Some(&cached), &current),
            "下载完成必须触发重扫"
        );
    }

    /// 目录 mtime 变化（用户往目录里丢了个自备模型 / 删掉一个）→ 重扫。
    ///
    /// 这是本次改造的关键：旧键只看 `ITEMS` 里登记条目的就位与否，**看不出**清单外
    /// 的文件增减——而自备模型恰恰都在清单外。
    #[test]
    fn models_snapshot_rescans_when_dir_mtime_changes() {
        let cached = ModelsKey {
            downloading: false,
            dir_mtime: Some((100, 0)),
        };
        let current = ModelsKey {
            downloading: false,
            dir_mtime: Some((200, 0)),
        };
        assert!(
            models_snapshot_stale(Some(&cached), &current),
            "目录内容变了必须重扫"
        );
    }

    /// 目录从「不存在」变为「存在」也要重扫：用户新建 `models/whisper` 并放模型，
    /// 不重扫就会一直显示空列表。
    #[test]
    fn models_snapshot_rescans_when_dir_appears() {
        let cached = ModelsKey {
            downloading: false,
            dir_mtime: None,
        };
        let current = ModelsKey {
            downloading: false,
            dir_mtime: Some((100, 0)),
        };
        assert!(
            models_snapshot_stale(Some(&cached), &current),
            "目录出现必须重扫"
        );
    }
}
