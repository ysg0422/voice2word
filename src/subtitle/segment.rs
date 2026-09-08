//! 字幕片段数据结构

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub index: usize,
    /// 起始时间 (秒)
    pub start: f64,
    /// 结束时间 (秒)
    pub end: f64,
    /// 原始转写识别文本
    pub text: String,
    /// LLM 润色文本 (若润色则使用，否则显示原文本)
    pub polished: String,
    /// 语种 (zh, en 等)
    pub language: Option<String>,
}

impl Segment {
    pub fn new(index: usize, start: f64, end: f64, text: impl Into<String>) -> Self {
        Self {
            index,
            start,
            end,
            text: text.into(),
            polished: String::new(),
            language: None,
        }
    }

    /// 显示文本：优先使用润色后文本，若为空则返回原文
    pub fn display_text(&self) -> &str {
        if !self.polished.trim().is_empty() {
            &self.polished
        } else {
            &self.text
        }
    }

    /// 片段时长 (秒)
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}
