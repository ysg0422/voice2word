pub mod audit;
pub mod cleanup;
pub mod clips;
pub mod dub;
pub mod edit;
pub mod fcpxml;
pub mod jianying;
pub mod json;
pub mod premiere;
pub mod qc;
pub mod reader;
pub mod segment;
pub mod stats;
pub mod timing;
pub mod ttml;
pub mod writer;
pub mod xml_util;

/// 导出工程文件（FCPXML / Premiere XML / 剪映草稿）时使用的画布与帧率。
///
/// 此前三个导出器各自硬编码 `30fps / 1920×1080`：非 30fps 素材进剪辑软件会出现
/// 帧量化漂移，竖屏素材则直接落进横屏画布。调用方应先用源媒体探测出真实参数，
/// 探测不到（音频、损坏文件）时回落到 [`ProjectFormat::default`]。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectFormat {
    pub fps: f64,
    pub width: u32,
    pub height: u32,
}

impl Default for ProjectFormat {
    fn default() -> Self {
        Self {
            fps: 30.0,
            width: 1920,
            height: 1080,
        }
    }
}

impl ProjectFormat {
    /// 构造并收敛到合理范围：异常帧率回落到 30，尺寸下限 16。
    pub fn new(fps: f64, width: u32, height: u32) -> Self {
        let fps = if fps.is_finite() && fps > 0.0 && fps <= 240.0 {
            fps
        } else {
            30.0
        };
        Self {
            fps,
            width: width.max(16),
            height: height.max(16),
        }
    }
}

pub use dub::{mix_track, plan_placements, required_rate_percent, DubPlacement, TRIM_GUARD_SECS};
pub use fcpxml::FcpXmlExporter;
pub use jianying::JianYingExporter;
pub use json::JsonSubtitleExporter;
pub use premiere::PremiereXmlExporter;
pub use qc::{
    ends_with_terminal_punctuation, missing_terminal_punctuation, plan_punctuation_fix,
    PunctuationPlan,
};
pub use segment::{
    contains_japanese_kana, contains_kana_or_hangul, contains_korean_hangul, dominant_language,
    glossary_violations, indices_cover_segments, is_source_chinese, language_name, matched_indices,
    next_issue_in, optimize_segments, plan_time_edit, reindex_split_pieces, split_long_segments,
    timeline_end, timeline_end_or, ExportMode, QualityCategory, Segment, TimeEdit, MIN_EDIT_DUR,
    MIN_SEGMENT_DUR,
};
pub use stats::{cps_thresholds, visible_char_count};
pub use timing::{
    cps_violations, plan_cps_fix, plan_min_gaps, CpsFixPlan, GapPlan, DEFAULT_MAX_CPS,
    DEFAULT_MIN_GAP_SECS,
};
pub use ttml::{TtmlExporter, TtmlProfile};
pub use writer::SubtitleWriter;
