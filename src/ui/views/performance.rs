//! 性能与推理设置工作台视图
//!
//! 实现「硬件检测 → 性能评估 → 策略矩阵决策 → 一键应用」的原生 GPUI 交互面板。
//! 此功能为实验性功能。

use gpui::prelude::*;
use gpui::*;
use std::sync::Mutex;
use std::time::Duration;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;
use crate::utils::config::{ModelInfo, TranslateConfig};

/// 术语表**实际生效**的条数（纯函数，便于单测）。
///
/// `TranslateConfig::glossary_entries()` 返回解析出的全部合法条目，而真正注入
/// 提示词的只有前 `TranslateConfig::MAX_GLOSSARY_ENTRIES` 条（见
/// `utils::config::glossary_prompt` 的 `.take(..)`）。界面直接显示解析条数
/// 会让用户以为全部生效——这里统一按实际生效数展示，超限时另行提示。
pub(crate) fn effective_glossary_count(parsed: usize, limit: usize) -> usize {
    parsed.min(limit)
}

/// ONNX 执行后端在性能页「引擎选择」卡上的一行状态：(文案, 是否警告色)。
///
/// 抽成纯函数是为了单测：三种状态里的「实况」只有真跑一次 runner 才会出现
/// （请求 `dml` 被 Python 侧回落成 `cpu`），单测里造不出来，所以这里只留
/// 「实况 → 文案」这一步映射，判定规则与日志层完全同源。
///
/// - `requested`：配置里请求的 provider。`GpuConfig::resolve_onnx_provider` 已把
///   `dml` / `directml` / 其它值归一成 `dml` 或 `cpu`，这里只对空串兜底。
/// - `actual`：runner 回报的实际生效 provider；`None` = 本次会话还没跑过 ONNX 推理。
/// - `engine_mounted`：SenseVoice / CT-Punc 至少挂载了一个。两者都没就绪时 provider
///   根本没机会生效，此时说「回落」是误导。
///
/// 返回 `true` 表示这一行要用警告色（琥珀）：唯一命中条件是「请求了非 cpu、实际却
/// 落到别的值」，与 `engines::sensevoice::provider_fallback_notice` 同一条规则。
pub(crate) fn provider_status_line(
    requested: &str,
    actual: Option<&str>,
    engine_mounted: bool,
) -> (String, bool) {
    if !engine_mounted {
        return ("未启用（SenseVoice / CT-Punc 均未就绪）".to_string(), false);
    }
    // 空串兜底成 cpu：与 `resolve_onnx_provider` 的「其余一律 cpu」一致，
    // 保证这个纯函数对任意输入都有确定输出。
    let configured = match requested.trim() {
        "" => "cpu",
        other => other,
    };
    let Some(actual) = actual.map(str::trim) else {
        // 还没回报实况：只回显配置值，绝不显示成已生效——用户配了 dml 却还没转过，
        // 看到光秃秃的 dml 很容易以为加速已经在跑。
        return (format!("{configured}（尚未跑过推理）"), false);
    };
    if crate::engines::sensevoice::provider_fallback_notice(configured, actual).is_some() {
        return (format!("配置 {configured}，实际 {actual}（已回落）"), true);
    }
    if configured == "cpu" {
        // 中性文案顺手解释「为什么不是 dml」，否则用户会以为自己漏了设置
        return ("CPU（未配置加速）".to_string(), false);
    }
    (format!("{configured}（实际生效）"), false)
}

// ==================== 在线翻译：模型列表拉取 ====================

/// 「拉取模型列表」的结果缓存。
///
/// 为什么不挂在 `MainWindow` 上：本文件的改动范围只到 `performance.rs`，`ui/mod.rs`
/// 里的窗口字段不能动。列表是进程级的临时数据（拉一次用一阵），放静态量不影响正确性：
/// 渲染时按 `(api_base, api_key)` 指纹比对，配置一改旧列表立即作废，不会拿过期结果
/// 去覆盖用户新填的基址。
static MODEL_LIST: Mutex<Option<ModelListCache>> = Mutex::new(None);

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
        "(空响应体)".to_string()
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
            .map_err(|e| ModelFetchError::transport(format!("读取 {url} 的响应失败: {e}"))),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let detail = crate::utils::config::gateway_error_summary(&body)
                .unwrap_or_else(|| body_snippet(&body));
            Err(ModelFetchError {
                status: Some(code),
                detail,
                raw: format!("在线接口返回 HTTP {code}: {body}"),
            })
        }
        Err(e) => Err(ModelFetchError::transport(format!("无法连接 {url}: {e}"))),
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

/// 从管线里取 ONNX 后端的**实况**（请求值, 实际生效值）。`None` = 尚无引擎回报。
///
/// # 为什么现在恒返回 `None`
///
/// 引擎侧已经齐了：`SenseVoiceEngine::provider_status()`（`src/engines/sensevoice.rs:259`）
/// 与 `PunctuationEngine::provider_status()`（`src/engines/punc.rs:146`）都是 `pub`，
/// 各自只读一个 `Mutex<Option<ProviderStatus>>`。缺的是从 `TaskPipeline` 到它们的
/// 那一跳：`pipeline.rs` 里持有引擎的 `sensevoice` / `punc` 两个字段是**私有**的
/// （`src/core/pipeline.rs:172` / `:174`），`TaskPipeline` 也没有导出任何读它们的
/// `pub` 方法。
///
/// # 单点接线（`src/core/pipeline.rs` 不在本文件的改写范围内，故只留这一处）
///
/// 在该文件的 `set_llm_threads`（当前在 `:244-246`）之后插入：
///
/// ```ignore
/// /// ONNX 执行后端实况：SenseVoice 优先，回退 CT-Punc；两者都没回报过则是 None。
/// ///
/// /// 供性能页把「配置了 dml 却回落 cpu」显示到界面上；引擎不感知 UI。
/// pub fn onnx_provider_status(&self) -> Option<ProviderStatus> {
///     self.sensevoice
///         .as_ref()
///         .and_then(|e| e.provider_status())
///         .or_else(|| self.punc.as_ref().and_then(|e| e.provider_status()))
/// }
/// ```
///
/// 接线后把下面那行 `None` 换成
/// `pipeline.onnx_provider_status().map(|s| (s.requested, s.actual))` 即可。
/// 本文件其余部分（渲染、配色、单测）均无需改动。
fn onnx_provider_actual(pipeline: &crate::core::TaskPipeline) -> Option<(String, String)> {
    pipeline
        .onnx_provider_status()
        .map(|s| (s.requested, s.actual))
}

impl MainWindow {
    /// 渲染性能与推理设置工作台
    pub(crate) fn render_performance_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // 页面外壳统一走 primitives::page_shell：内边距 / 分区间距 / 标题字号
        // 与其余三个工作台页同源，切页时内容不再横向跳动。
        primitives::page_shell("performance-settings-page")
            // 1. 页头：标题 + 帮助按钮
            .child(self.render_page_header(cx))
            // 2. 推荐配置：硬件评估档位 + 用户推理偏好 + 一键套用（真正落盘）
            .child(self.render_recommend_card(cx))
            // 3. 配置说明卡 (右上角问号按钮展开)
            .children(if self.state.show_perf_help {
                Some(self.render_perf_help_card(cx))
            } else {
                None
            })
            // 4. 步骤 2: 识别引擎与模型架构选择
            .child(self.render_engine_selection_card(cx))
            // 5. 步骤 3: 并行度（线程数 / 进程数滑条，量程按本机核心数推导）
            .child(self.render_parallelism_card(cx))
            // 6. 步骤 4: 标点恢复与 AI 文本润色
            .child(self.render_polish_selection_card(cx))
            // 7. 步骤 5: 字幕翻译引擎（离线 Qwen / 在线 OpenAI 兼容 API）
            .child(self.render_translate_settings_card(cx))
            // 8. 步骤 6: 模型与外部组件（缺什么、一键补齐；全部走国内镜像）
            .child(self.render_model_manager(false, cx))
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
            .gap_2()
            .w_full()
            .child(key_input)
            .child(
                // 「显示 / 隐藏」密钥：外观与 chip 原语同形同色，直接复用
                primitives::chip(if key_visible { "隐藏" } else { "显示" }, false, false)
                    .id("api-key-visible-toggle")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.api_key_visible = !this.api_key_visible;
                        cx.notify();
                    })),
            )
            .into_any_element();

        // 批量条数：在线接口按 token 计费，批量越大往返越少；本地推理则影响不大
        let batch = self.state.config.translate.batch_size;
        let batch_control = primitives::segmented_cluster()
            .children([10usize, 20, 40].into_iter().map(|size| {
                self.seg_option(
                    match size {
                        10 => "perf-translate-batch-10",
                        20 => "perf-translate-batch-20",
                        _ => "perf-translate-batch-40",
                    },
                    match size {
                        10 => "10 条/批",
                        20 => "20 条/批",
                        _ => "40 条/批",
                    },
                    batch == size,
                    cx,
                    move |this, cx| {
                        this.state.config.translate.batch_size = size;
                        this.state.save_translate_config();
                        cx.notify();
                    },
                )
            }))
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
                div()
                    .id("perf-translate-probe-btn")
                    .px(px(Theme::SPACE_4))
                    .h(px(Theme::CTRL_H_SM))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Theme::RADIUS_LG))
                    .bg(if probing {
                        Theme::bg_disabled()
                    } else {
                        Theme::bg_panel()
                    })
                    .border_1()
                    .border_color(Theme::bg_hover_strong())
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if probing {
                        Theme::text_disabled()
                    } else {
                        Theme::accent_blue()
                    })
                    .when(!probing, |s| {
                        s.cursor_pointer()
                            .hover(|s| s.bg(Theme::bg_track()).text_color(Theme::accent_blue()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.probe_online_translate_api(cx);
                            }))
                    })
                    .child(if probing {
                        "测试中…"
                    } else {
                        "测试连接"
                    }),
            )
            .into_any_element();

        // 术语表：多行文本，用外部编辑器改（自绘单行框装不下）。两档引擎都生效。
        // 解析出的条目数包含**全部**合法条目，但注入提示词时只取前
        // `TranslateConfig::MAX_GLOSSARY_ENTRIES` 条（见 `utils::config::glossary_prompt`）。
        // 显示解析条数会让用户以为「120 条全在生效」，实际只有前 80 条——
        // 这里按实际生效数显示，并在触到上限时明确提示被截断。
        let glossary_count = self.state.config.translate.glossary_entries().len();
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
                        "未设置（可留空）".to_string()
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

        primitives::card_rows()
            .child(Self::render_setting_row("翻译引擎", mode_control))
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
                    .child(
                        div()
                            .w_full()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child(TranslateConfig::MODEL_ID_HELP),
                    )
                    .child(model_fetch_row)
                    .children(model_choice_row)
                    .into_any_element())),
            )
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("API Key", dim(key_control)))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("每批条数", dim(batch_control)))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("连通性", probe_control))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("术语表", glossary_control))
    }

    /// 页头：标题 + 帮助按钮 (简约，无徽标)
    fn render_page_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let show_help = self.state.show_perf_help;
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .child(primitives::page_title("性能与推理设置"))
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
                        Theme::bg_hover_strong()
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
            // 推荐档位可能改写 config.paths.whisper_model，下方「模型与组件」卡的
            // 就位徽标缓存要跟着重算，否则会显示成上一档位的结论。
            this.state.refresh_model_presence();
            if let Err(e) = this.state.config.save_to_file("config.toml") {
                this.notice = Some(format!("推荐配置已生效，但写入 config.toml 失败: {e}"));
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
            || self.state.config.pipeline.parallel_workers != 0;
        let summary = format!(
            "{}：{} · {} 线程 · 并发 {}（{}）",
            if drifted {
                "当前已偏离推荐"
            } else {
                "当前即为推荐"
            },
            profile.whisper_model_name,
            profile.whisper_threads,
            profile.max_concurrency,
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
                    .child("套用会把推荐档位 / 线程数 / 并行进程数 / VAD 开关写进 config.toml，覆盖你在下方手动调整的取值（对下一次转写生效）。"),
            )
    }

    /// 配置说明卡：由右上角问号按钮展开，逐条介绍各配置项的作用
    fn render_perf_help_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let items: [(&'static str, &'static str); 7] = [
            (
                "转写引擎",
                "SenseVoice：中文极速、自带标点；Whisper：多语种、方言口音更稳。",
            ),
            (
                "Whisper 模型档位",
                "Base 最快 → Turbo Q8 最精，按「速度换精度」选择，倍速为相对实时倍率。",
            ),
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
                    .gap_2()
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
            let _ = this.update(cx, |_this, cx| {
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

    /// 模型下拉所在行的「行尾」内容：拉取按钮 + 结果状态小字。
    fn render_model_fetch_row(&self, is_online: bool, cx: &mut Context<Self>) -> AnyElement {
        const MODEL_FETCH_MSG_MAX_W: f32 = 420.0;
        let cfg = self.state.config.translate.clone();
        let (loading, status, models) = model_cache_snapshot(&cfg);
        let model_selected = self.state.config.translate.api_model.trim().to_string();
        let has_selection = models.iter().any(|m| m.id == model_selected);

        let mut button = primitives::chip(
            if loading {
                "拉取中…"
            } else {
                "拉取模型列表"
            },
            false,
            false,
        )
        .id("perf-translate-models-btn");
        if !loading && is_online {
            button = button
                .cursor_pointer()
                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                .on_click(cx.listener(|this, _, _, cx| this.fetch_translate_models(cx)));
        }

        let mut row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(Theme::SPACE_2))
            .child(button);
        if let Some((ok, text)) = status {
            row = row.children(text.lines().map(move |line| {
                div()
                    .max_w(px(MODEL_FETCH_MSG_MAX_W))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(if ok {
                        Theme::accent_mint()
                    } else {
                        Theme::accent_red()
                    })
                    .child(line.to_string())
            }));
        }
        // 列表拉到了、但当前模型名不在列表里：直接提示，免得用户拿着一个
        // 会 400 的 id 去点「测试连接」
        if !loading && !models.is_empty() && !has_selection {
            row = row.child(
                div()
                    .max_w(px(MODEL_FETCH_MSG_MAX_W))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::accent_orange())
                    .child("当前模型名不在服务端列表内，点下方的 id 直接替换"),
            );
        }
        row.into_any_element()
    }

    /// 服务端模型的下拉列表：点击即写入模型名（复用 `segmented` 范式，不自造样式）。
    fn render_model_choice_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let (loading, _, models) = model_cache_snapshot(&self.state.config.translate);
        if loading || models.is_empty() {
            return div().into_any_element();
        }
        let selected = self.state.config.translate.api_model.trim().to_string();
        let mut row = div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap(px(Theme::SPACE_1_5))
            .pb(px(Theme::SPACE_2));
        for model in models {
            let id = model.id.clone();
            let is_sel = id == selected;
            let mut pill = primitives::segmented(model.display_label(), is_sel, false)
                .id(SharedString::from(format!("perf-translate-model-{id}")))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.apply_translate_model(&id, cx);
                }));
            if let Some(tag) = model.reasoning_tag() {
                // 推理模型标记：推理模型翻译更慢、更吃输出预算，正是用户踩过的坑
                pill = pill.child(
                    div()
                        .ml(px(Theme::SPACE_1))
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(if is_sel {
                            Theme::accent_blue()
                        } else {
                            Theme::accent_orange()
                        })
                        .child(tag),
                );
            }
            row = row.child(pill);
        }
        row.into_any_element()
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

    /// ONNX 执行后端在性能页上的落点：挂在「引擎选择」卡的最后一行。
    ///
    /// 三态与配色（琥珀色与 `editor.rs` 的术语违规行同源）：
    /// - 未回落（provider 就是 cpu）→ 中性灰 `text_muted`；
    /// - 回落（配置 dml、实际 cpu）→ 琥珀 `tint_warn_soft` 底 + `accent_orange` 字，
    ///   下面再跟一行小字可操作指引；
    /// - 未启用（SenseVoice / CT-Punc 都没就位）→ 中性灰，并在文案里写明原因。
    ///
    /// 为什么自己写一行、而不是走 `render_setting_row`：这里的控件是纯说明文字 + 会
    /// 换行的指引，塞进那个「左标签 + 右对齐控件」的骨架里会被折成右对齐的窄条；
    /// 左对齐一列与卡片里其它长说明小字同一观感（写法参照 `render_recommend_card`
    /// 底部的覆盖告知小字）。
    ///
    /// 「是否就位」用的是 `model_present` 缓存快照（`AppState::refresh_model_presence`
    /// 填充），只查表不碰磁盘——和本页「模型与组件」卡同一套判定，避免逐帧 stat。
    ///
    /// 代价：实况那次读取每帧要取一次引擎侧 `Mutex`（`provider_status()`），但锁内只是
    /// 一次 `Option<ProviderStatus>` 克隆，且本页只在切到该页时渲染，开销可忽略，
    /// 因此不做跨帧缓存。
    fn render_onnx_provider_row(&self) -> Div {
        let (value, fallback) = provider_status_line(
            self.state.config.gpu.resolve_onnx_provider(),
            onnx_provider_actual(&self.state.pipeline)
                .map(|(_, actual)| actual)
                .as_deref(),
            self.state.model_is_present("sensevoice-model")
                || self.state.model_is_present("punc-model"),
        );

        let row = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_4))
            .py(px(Theme::SPACE_2))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_BODY_LG))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_primary())
                    .child("ONNX 后端"),
            )
            .child(if fallback {
                // 琥珀：与 `editor.rs` 术语违规行同源（`tint_warn_soft` / `accent_orange`）
                primitives::tag_tinted(
                    value,
                    Theme::tint_warn_soft(),
                    Theme::tint_warn_border(),
                    Theme::accent_orange(),
                )
                .max_w(px(Theme::PROBE_MSG_MAX_W))
                .into_any_element()
            } else {
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(value)
                    .into_any_element()
            });

        if !fallback {
            return row;
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .child(row)
            .child(
                // 日志里那条告警太长，这里只留可操作的那两句；用最小字号单独一行，
                // 免得把卡片行高撑乱
                div()
                    .w_full()
                    .pb(px(Theme::SPACE_2))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::accent_orange())
                    .child("按日志指引修复后重启生效：`pip install onnxruntime-directml` 并改用 DirectML 版 sherpa-onnx；不打算升级就把 provider 切回 `cpu`。"),
            )
    }

    /// 步骤 2：识别引擎与模型架构选择 (每行一个配置项，右侧单行等级选择)
    fn render_engine_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_sv = self.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

        let engine_control = primitives::segmented_cluster()
            .child(self.seg_option(
                "perf-engine-pill-sv",
                "SenseVoice 极速 (推荐)",
                is_sv,
                cx,
                |this, cx| {
                    this.state.whisper_model_tier = crate::app::WhisperModelTier::SenseVoice;
                    cx.notify();
                },
            ))
            .child(self.seg_option(
                "perf-engine-pill-whisper",
                "OpenAI Whisper 全能",
                !is_sv,
                cx,
                |this, cx| {
                    if this.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice {
                        this.state.whisper_model_tier = crate::app::WhisperModelTier::TurboSpeed;
                    }
                    cx.notify();
                },
            ))
            .into_any_element();

        let tiers: [(crate::app::WhisperModelTier, &'static str, &'static str); 4] = [
            (
                crate::app::WhisperModelTier::Fast,
                "perf-tier-base",
                "Base · 20x",
            ),
            (
                crate::app::WhisperModelTier::Balanced,
                "perf-tier-small",
                "Small-Q5 · CPU",
            ),
            (
                crate::app::WhisperModelTier::TurboSpeed,
                "perf-tier-turboq5",
                "Turbo Q5 · 6x",
            ),
            (
                crate::app::WhisperModelTier::Precise,
                "perf-tier-turboq8",
                "Turbo Q8 · 4x",
            ),
        ];
        let mut tier_row = primitives::segmented_cluster();
        for (tier, id, label) in tiers {
            let is_sel = self.state.whisper_model_tier == tier;
            tier_row = tier_row.child(self.seg_option(id, label, is_sel, cx, move |this, cx| {
                this.state.whisper_model_tier = tier;
                cx.notify();
            }));
        }
        let tier_control = if is_sv {
            tier_row.opacity(0.5).into_any_element()
        } else {
            tier_row.into_any_element()
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(Theme::PAGE_GAP))
            .child(
                primitives::card_rows()
                    .child(Self::render_setting_row("转写引擎", engine_control))
                    .child(Self::render_setting_divider())
                    .child(Self::render_setting_row("Whisper 模型档位", tier_control))
                    .child(Self::render_setting_divider())
                    .child(self.render_onnx_provider_row()),
            )
    }

    /// 步骤 3：并行度设置（转写线程数 / 并行进程数 / 润色线程数）
    ///
    /// 三项都用滑条，量程按本机核心数推导：线程数上限 = 逻辑核心数；
    /// 进程数上限 = 引擎内实测最优的并行进程数（`sensevoice_worker_count`），
    /// 最左档为「自动」，即按核数推导。改动即时写入 config.toml 并作用于下一次任务。
    fn render_parallelism_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cores = crate::app::AppState::logical_cores();
        let thread_max = cores.max(2);
        let thread_val = self.state.whisper_threads.clamp(2, thread_max);

        let auto_procs = crate::engines::sensevoice_worker_count(cores) as u32;
        let proc_max = auto_procs.max(1);
        let proc_val = self.state.config.pipeline.parallel_workers.min(proc_max);

        let thread_hint = format!(
            "本机 {cores} 逻辑核心 · SenseVoice 单会话超过 8 线程后收益递减，Whisper 建议设为物理核心数"
        );
        let proc_hint = if proc_val == 0 {
            format!("自动：按本机 {cores} 核推导为 {auto_procs} 进程（实测最优）")
        } else {
            format!(
                "本机 {cores} 核，自动值为 {auto_procs} 进程 · 仅长音频（≥3 分钟）多进程切块时生效"
            )
        };

        let thread_slider = self.render_slider(
            "perf-slider-threads",
            "转写线程数",
            thread_val,
            2,
            thread_max,
            format!("{thread_val} 线程"),
            thread_hint,
            cx,
            move |this, v, cx| {
                if this.state.whisper_threads != v {
                    this.state.whisper_threads = v;
                    this.state.config.pipeline.whisper_threads = v;
                    let _ = this.state.config.save_to_file("config.toml");
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
            if proc_val == 0 {
                format!("自动（{auto_procs}）")
            } else {
                format!("{proc_val} 进程")
            },
            proc_hint,
            cx,
            move |this, v, cx| {
                if this.state.config.pipeline.parallel_workers != v {
                    this.state.config.pipeline.parallel_workers = v;
                    this.state.pipeline.set_parallel_workers(v as usize);
                    let _ = this.state.config.save_to_file("config.toml");
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
            format!("本机 {cores} 逻辑核心 · 供 Qwen 深度润色使用，选「极速标点」时不受影响"),
            cx,
            move |this, v, cx| {
                if this.state.config.pipeline.llm_threads != v {
                    this.state.config.pipeline.llm_threads = v;
                    this.state.pipeline.set_llm_threads(v);
                    let _ = this.state.config.save_to_file("config.toml");
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
    fn render_slider(
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
            .flex_col()
            .gap(px(Theme::SPACE_2))
            .py(px(Theme::SPACE_2))
            // 上行：标签 + 当前值胶囊
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY_LG))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(label),
                    )
                    .child(primitives::badge_accent(value_text)),
            )
            // 轨道
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(Theme::SPACE_6))
                    .flex()
                    .items_center()
                    // 1) 底轨 + 已选填充
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
                    // 2) 手柄：外圈用卡片底色形成描边，内圈实心薄荷色
                    .child(
                        div()
                            .absolute()
                            .left(relative(ratio))
                            .ml(px(-8.0))
                            .w(px(Theme::SPACE_4))
                            .h(px(Theme::SPACE_4))
                            .rounded_full()
                            .bg(Theme::bg_card())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .w(px(Theme::SPACE_2))
                                    .h(px(Theme::SPACE_2))
                                    .rounded_full()
                                    .bg(Theme::accent_mint()),
                            ),
                    )
                    // 3) 命中层：每档一个隐形格子，支持单击与按住拖动
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
                                    .h_full()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| on_change(this, v, cx)),
                                    )
                                    .on_mouse_move(cx.listener(
                                        move |this, event: &MouseMoveEvent, _, cx| {
                                            if event.pressed_button == Some(MouseButton::Left) {
                                                on_change(this, v, cx);
                                            }
                                        },
                                    ))
                            })),
                    ),
            )
            // 提示
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(hint),
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
            this.state.enable_polish = !this.state.enable_polish;
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
                    // 点选润色引擎 = 明确要润色，顺手打开总开关
                    this.state.enable_polish = true;
                    this.state.polish_mode = crate::app::PolishMode::PuncFast;
                    cx.notify();
                },
            ))
            .child(self.seg_option(
                "perf-select-qwen-deep",
                "Qwen 深度润色",
                !is_punc && enable_polish,
                cx,
                |this, cx| {
                    this.state.enable_polish = true;
                    this.state.polish_mode = crate::app::PolishMode::QwenDeep;
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
    use super::effective_glossary_count;
    use crate::utils::config::TranslateConfig;

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

    /// 回归（P3）：ONNX provider 回落的界面文案必须与日志层的判定完全同源——
    /// 「请求 dml、实际 cpu」是唯一需要警告色的情形；请求本来就是 cpu 时不该报警。
    #[test]
    fn onnx_provider_line_covers_normal_fallback_and_disabled() {
        use super::provider_status_line;

        // 未回落 · 正常：请求就是 cpu（默认，零回归）
        assert_eq!(
            provider_status_line("cpu", Some("cpu"), true),
            ("CPU（未配置加速）".to_string(), false)
        );
        // 未回落 · 正常：请求 dml 且真的吃到了
        assert_eq!(
            provider_status_line("dml", Some("dml"), true),
            ("dml（实际生效）".to_string(), false)
        );
        // 回落：请求 dml、runner 回报 cpu —— 这一档才用警告色
        assert_eq!(
            provider_status_line("dml", Some("cpu"), true),
            ("配置 dml，实际 cpu（已回落）".to_string(), true)
        );
        // 未启用：SenseVoice / CT-Punc 都没挂载
        assert_eq!(
            provider_status_line("dml", None, false),
            ("未启用（SenseVoice / CT-Punc 均未就绪）".to_string(), false)
        );
        // 边界：引擎在、但本次会话还没跑过 ONNX 推理
        assert_eq!(
            provider_status_line("dml", None, true),
            ("dml（尚未跑过推理）".to_string(), false)
        );
        // 边界：配置为空串时按 cpu 处理（与 `resolve_onnx_provider` 同一收敛规则）
        assert_eq!(
            provider_status_line("", Some("cpu"), true),
            ("CPU（未配置加速）".to_string(), false)
        );
    }
    /// 端到端证据（`#[ignore]`，不进常规 `cargo test --lib`）：
    ///
    /// 本机 onnxruntime 只装了 CPU provider（实测
    /// `['AzureExecutionProvider','CPUExecutionProvider']`），所以**只要真跑一次**标点
    /// runner 并请求 `directml`，sherpa-onnx 就会在原生日志里回落，runner 也会如实
    /// 在 stdout 回报 `{"requested":"directml","actual":"cpu"}`。这里把这条实况喂进
    /// `provider_status_line`，证明界面文案确实会翻成琥珀色的「已回落」。
    ///
    /// 依赖 Python 侧 sherpa-onnx；跑法：
    /// `cargo test --lib -- --ignored onnx_provider_fallback_is_visible_end_to_end`
    #[test]
    #[ignore = "端到端实测：需要本机 Python + sherpa-onnx 与 models/punc 模型"]
    fn onnx_provider_fallback_is_visible_end_to_end() {
        use super::provider_status_line;
        use crate::engines::PunctuationEngine;
        use crate::subtitle::Segment;

        let model = std::path::PathBuf::from("models/punc/model.int8.onnx");
        let runner = std::path::PathBuf::from("tools/punc_runner.py");
        assert!(runner.exists(), "缺 runner: {runner:?}");
        assert!(model.exists(), "缺标点模型: {model:?}（先跑一次模型下载）");

        let engine =
            PunctuationEngine::with_python_and_provider(&runner, &model, 4, "python", "dml");
        let segs = vec![Segment::new(1, 0.0, 2.0, "今天天气不错我们出去走走吧")];
        let out = engine
            .add_punctuation(segs, None)
            .expect("标点 runner 应能跑通");
        assert_eq!(out.len(), 1);

        let status = engine
            .provider_status()
            .expect("runner 必须回报 provider 事件");
        let (line, fallback) =
            provider_status_line(&status.requested, Some(status.actual.as_str()), true);
        // 本机无 DirectML：实况必然是 requested=directml / actual=cpu，界面该报琥珀色
        assert_eq!(status.actual, "cpu", "本机只有 CPU provider，实况应是 cpu");
        assert!(fallback, "回落必须点亮警告色，实际文案: {line}");
        assert_eq!(
            line,
            format!(
                "配置 {}，实际 {}（已回落）",
                status.requested, status.actual
            )
        );
    }
}
