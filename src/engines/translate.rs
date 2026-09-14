//! 字幕翻译引擎 — 支持离线 Qwen 大模型字幕翻译及在线 API 极速翻译

use anyhow::Result;
use tracing::info;
use crate::subtitle::Segment;
use crate::engines::LLMEngine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateMode {
    /// 本地 Qwen 大模型离线翻译 (免费、无网可用)
    OfflineQwen,
    /// 在线 API 兼容格式 (OpenAI / DeepSeek 等)
    OnlineApi,
}

pub struct TranslateEngine {
    llm_engine: LLMEngine,
}

impl TranslateEngine {
    pub fn new(llm_engine: LLMEngine) -> Self {
        Self { llm_engine }
    }

    /// 批量翻译字幕片段为目标语言（默认 "简体中文"）
    pub fn translate_subtitles(
        &self,
        segments: Vec<Segment>,
        target_lang: &str,
        progress_cb: Option<Box<dyn Fn(f64, &str) + Send>>,
    ) -> Result<Vec<Segment>> {
        info!("开始执行字幕翻译 (目标语言: {})", target_lang);
        self.llm_engine.translate(segments, target_lang, progress_cb)
    }
}
