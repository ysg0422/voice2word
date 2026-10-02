//! 字幕翻译引擎 — 支持离线 Qwen 大模型字幕翻译及在线 API 极速翻译
//!
//! 两条链路共用同一套「逐行 `[序号] 译文`」协议与解析器：
//! - `OfflineQwen`：本地 llama.cpp + Qwen 小模型，免费、断网可用、无需密钥；
//! - `OnlineApi`：任意 OpenAI 兼容的 `/chat/completions` 接口
//!   （DeepSeek / OpenAI / 通义 / Kimi / 本地 vLLM / Ollama 均可）。

use std::collections::{HashMap, HashSet};
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
    ) -> Result<Vec<Segment>> {
        info!("开始执行字幕翻译 (目标语言: {}, 引擎: {:?})", target_lang, self.mode);
        match self.mode {
            TranslateMode::OfflineQwen => {
                let engine = self
                    .llm_engine
                    .as_ref()
                    .ok_or_else(|| anyhow!("离线翻译引擎未初始化"))?;
                engine.translate(segments, target_lang, progress_cb)
            }
            TranslateMode::OnlineApi => {
                let cfg = self
                    .online
                    .as_ref()
                    .ok_or_else(|| anyhow!("在线翻译接口未配置"))?;
                translate_via_online_api(cfg, segments, target_lang, progress_cb)
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
            "max_tokens": 16,
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

    let total = segments.len();
    let batch_size = cfg.batch_size.clamp(1, 60);
    info!(
        "在线 API 翻译 {} 条字幕为 {}（模型 {}，每批 {} 条）",
        total, target_lang, cfg.model, batch_size
    );
    if let Some(ref cb) = progress_cb {
        cb(0.0, &format!("正在连接在线翻译接口 ({})...", cfg.model));
    }

    let mut completed = 0usize;
    // 只对有文本的片段建索引，空片段（纯静音/呼吸）不浪费额度
    let translatable: Vec<usize> = segments
        .iter()
        .enumerate()
        .filter(|(_, seg)| !seg.text.trim().is_empty())
        .map(|(pos, _)| pos)
        .collect();

    for chunk in translatable.chunks(batch_size) {
        let batch: Vec<&Segment> = chunk.iter().map(|&pos| &segments[pos]).collect();
        let translations = request_batch_translation(cfg, &batch, target_lang)?;
        let mut matched = 0usize;
        for &pos in chunk {
            if let Some(text) = translations.get(&segments[pos].index) {
                segments[pos].translation = Some(text.clone());
                matched += 1;
            }
        }
        if matched < chunk.len() {
            warn!(
                "在线翻译本批输出不完整 ({}/{} 条)，未匹配条目保持未翻译",
                matched,
                chunk.len()
            );
        }
        completed += chunk.len();
        if let Some(ref cb) = progress_cb {
            let progress = completed as f64 / total.max(1) as f64;
            cb(
                progress,
                &format!("在线翻译中: {}/{} 条（{}）", completed, total, cfg.model),
            );
        }
    }

    info!("在线 API 字幕翻译完成");
    Ok(segments)
}

/// 请求一批字幕的译文，返回 `序号 -> 译文` 映射
fn request_batch_translation(
    cfg: &OnlineApiConfig,
    batch: &[&Segment],
    target_lang: &str,
) -> Result<HashMap<usize, String>> {
    let source = batch
        .iter()
        .map(|seg| format!("[{}] {}", seg.index, seg.text.replace(['\r', '\n'], " ")))
        .collect::<Vec<_>>()
        .join("\n");

    let payload = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {
                "role": "system",
                "content": format!(
                    "你是专业字幕翻译专家。把用户给出的带序号字幕逐条翻译为地道的{target_lang}，\
                     保持原意与语气，语言通顺紧凑。\
                     必须逐行输出，格式严格为「[序号] 译文」，不得解释、不得合并、不得遗漏、不得改动序号。"
                )
            },
            { "role": "user", "content": source }
        ],
        "temperature": 0.2,
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
    Ok(LLMEngine::parse_batch_response(&content, &expected))
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
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
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
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
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
        let err = translate_via_online_api(&cfg, segs, "简体中文", None)
            .expect_err("空密钥必须直接报错而不是静默返回原文");
        assert!(err.to_string().contains("API Key"), "错误信息应指引用户去填密钥: {err}");
    }

    #[test]
    fn empty_model_fails_fast() {
        let cfg = OnlineApiConfig {
            model: "  ".to_string(),
            ..sample_cfg()
        };
        let segs = vec![Segment::new(1, 0.0, 1.0, "hello")];
        let err = translate_via_online_api(&cfg, segs, "简体中文", None)
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
    }
}
