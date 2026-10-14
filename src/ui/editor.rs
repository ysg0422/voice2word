//! 类似剪映 / Premiere 风格的多轨视频剪辑与字幕校对工作台
//! 提供：
//! 1. 顶部工作模式 Tab 切换 (智能生成 vs 剪辑校对)
//! 2. 视频画面同步监视器 (支持帧级预览与电影级字幕叠加)
//! 3. 字幕属性检查器 (支持实时修改错字、增删标点、时间微调、拆分与合并)
//! 4. 专业多轨时间轴 (时间刻度标尺、红/青色垂直指针游标、视频轨与字幕胶囊块)

use super::primitives;
use super::theme::Theme;
use super::{
    apply_line_edit, timeline_visible_positions, EditorExportFormat, EditorSubtitlePanel,
    MainWindow, ReplaceRuleScope, StyleField, TimelineIndex,
};
use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::subtitle::segment::WordTiming;
use crate::subtitle::{ExportMode, GapPlan, Segment};
use crate::utils::time::{format_duration_short, seconds_to_hms, seconds_to_timestamp_short};
use gpui::prelude::*;
use gpui::*;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

/// 替换面板里「正则 / 整词 / 范围」三项选择的会话态。
///
/// 为什么不挂在 `MainWindow` 上：这三项只被本模块使用，而窗口结构体定义在
/// `ui/mod.rs`，本模块不便向外扩字段。GPUI 的渲染与事件都在主线程执行，线程局部
/// 变量的可见性与窗口字段一致；面板收起再展开会保留上次选择，与「区分大小写 /
/// 含译文」两个既有开关的行为对齐。
struct ReplacePanelToggles {
    regex: std::cell::Cell<bool>,
    whole_word: std::cell::Cell<bool>,
    scope_selected: std::cell::Cell<bool>,
}

thread_local! {
    static REPLACE_PANEL_TOGGLES: ReplacePanelToggles = const {
        ReplacePanelToggles {
            regex: std::cell::Cell::new(false),
            whole_word: std::cell::Cell::new(false),
            scope_selected: std::cell::Cell::new(false),
        }
    };
}

/// 字幕配置 → 预览配色 `(文字色, 底色, 描边色)`。
///
/// 完全由 `SubtitleStyleConfig` 驱动，支持自定义文字颜色、描边粗细与底框不透明度。
fn subtitle_computed_colors(
    style: &crate::utils::SubtitleStyleConfig,
) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba) {
    let (r, g, b) = crate::utils::SubtitleStyleConfig::hex_to_rgb(&style.primary_color);
    let fg = rgb((r as u32) << 16 | (g as u32) << 8 | (b as u32));

    let (or, og, ob) = crate::utils::SubtitleStyleConfig::hex_to_rgb(&style.outline_color);
    let border = if style.outline_width > 0.0 {
        rgb((or as u32) << 16 | (og as u32) << 8 | (ob as u32))
    } else {
        Theme::transparent()
    };

    let bg = match style.bg_style.as_str() {
        "none" => Theme::transparent(),
        "box" => {
            let a = ((style.bg_opacity.clamp(0.0, 1.0) * 255.0) as u32).clamp(0, 255);
            rgba(a)
        }
        "pill" => {
            let a = ((style.bg_opacity.clamp(0.0, 1.0) * 230.0) as u32).clamp(0, 255);
            rgba(0x18181b00 | a)
        }
        _ => {
            // "shadow" 默认投影底色
            let a = ((style.bg_opacity.clamp(0.0, 1.0) * 160.0) as u32).clamp(0, 255);
            rgba(a)
        }
    };

    (fg, bg, border)
}

#[allow(dead_code)]
fn subtitle_preset_colors(preset: &str) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba) {
    let mut cfg = crate::utils::SubtitleStyleConfig::default();
    cfg.apply_preset(preset);
    subtitle_computed_colors(&cfg)
}

/// 样式预览条的底色修正。
///
/// 预览条里没有真实视频画面，浅色主题下衬底是浅灰；而「电影沉浸」预设本身不带底框、
/// 字形是恒白——白字压浅灰等于看不见。这里在**预览条**这个特殊语境下补一层深色
/// 底衬，只为让预览可读；监视器里压在真实画面上的字幕不受影响，仍走原预设。
fn preview_backdrop(bg: gpui::Rgba) -> gpui::Rgba {
    if Theme::is_light() && bg == Theme::transparent() {
        Theme::bg_overlay()
    } else {
        bg
    }
}

/// HSV(0..360, 0..1, 0..1) 转 RGB(0..255)
pub(crate) fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let h_prime = ((h % 360.0) + 360.0) % 360.0 / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = if (0.0..1.0).contains(&h_prime) {
        (c, x, 0.0)
    } else if (1.0..2.0).contains(&h_prime) {
        (x, c, 0.0)
    } else if (2.0..3.0).contains(&h_prime) {
        (0.0, c, x)
    } else if (3.0..4.0).contains(&h_prime) {
        (0.0, x, c)
    } else if (4.0..5.0).contains(&h_prime) {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    (
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// 字幕样式可选值（字号 / 底边距以 1080p 为基准，与 `SubtitleStyleConfig` 语义一致）
#[allow(dead_code)]
const STYLE_FONT_SIZES: [u32; 5] = [28, 36, 44, 52, 60];
#[allow(dead_code)]
const STYLE_LETTER_SPACINGS: [u32; 5] = [0, 1, 2, 4, 6];
#[allow(dead_code)]
const STYLE_LINE_SPACINGS: [f32; 5] = [1.0, 1.2, 1.4, 1.6, 1.8];
#[allow(dead_code)]
const STYLE_MAX_CHARS: [u32; 5] = [10, 14, 18, 22, 26];
#[allow(dead_code)]
const STYLE_BOTTOM_MARGINS: [u32; 5] = [20, 40, 60, 80, 120];
/// 分段按钮组的宽度上界：4 个短标签等分后每个约 117px，够放下 2~4 个字，
/// 又不会像铺满整行那样被拉伸成 200px 的空条。用 max_w 而非固定宽，
/// 是为了窄面板下仍能随容器收缩，避免「标签 + 按钮组」溢出卡片。
#[allow(dead_code)]
const STYLE_ROW_BTN_MAX_W: f32 = 480.0;

/// 左右二分布局下左侧视频监视器的最小宽度（逻辑 px）
const EDITOR_LEFT_MIN_W: f32 = 300.0;
/// 右侧字幕配置与列表区域的固定面板宽度（逻辑 px，类似 B 站 / YouTube 侧边栏，视频占绝大多数区域）
const EDITOR_RIGHT_PANEL_W: f32 = 400.0;

/// 窗口逻辑宽度低于此值时，「视频监视器 | 字幕配置」由左右二分改为上下堆叠。
/// 导航栏已上移至顶部，可用宽度不再减 180px。
const EDITOR_STACK_BELOW_W: f32 = 820.0;

/// 上下堆叠布局下视频监视器固定占用的高度（监视器自身最少需要 36+180+52=268）。
const EDITOR_STACK_MONITOR_H: f32 = 320.0;

/// 左右分栏时中间分隔条的宽度（与 `editor-split-divider` 的 `w` 保持一致）
const EDITOR_SPLIT_DIVIDER_W: f32 = 5.0;

/// 视频画面在监视器视口内屏里的最大占比。
///
/// # 为什么是 1.0（顶满），而不是留一圈白
///
/// 这里曾经是 0.78：四周人为留一圈对称留白。实际用下来是反效果——监视器的价值就是
/// 「看清这一帧写了什么」，而在 16:9 视频 + 宽视口的组合下，宽高比本身就已经在左右
/// 留出黑边了，再乘一个 0.78 就变成**四周同时**一圈黑框，画面被压到只有视口的一半
/// 多点，字小得看不清，用户第一反应是「怎么这么多黑框」。
///
/// 现在取满：黑边只由宽高比决定，不再由这个系数额外制造。留白靠视口自身的边距
/// （`monitor-viewport-screen`），不靠缩画面。
const MONITOR_VIDEO_FIT_FRAC: f32 = 1.0;

/// 在 `avail_w × avail_h` 的容器内按 `aspect`（宽/高）等比内接得到的最大尺寸。
/// 任一输入非有限或非正时返回 `(0.0, 0.0)`，交由调用方降级处理。
fn fit_video_size(aspect: f32, avail_w: f32, avail_h: f32) -> (f32, f32) {
    let invalid = !aspect.is_finite()
        || aspect <= 0.0
        || !avail_w.is_finite()
        || !avail_h.is_finite()
        || avail_w <= 0.0
        || avail_h <= 0.0;
    if invalid {
        return (0.0, 0.0);
    }
    let height_by_width = avail_w / aspect;
    if height_by_width <= avail_h {
        (avail_w, height_by_width)
    } else {
        (avail_h * aspect, avail_h)
    }
}

/// 点击字幕清单某一行的跳播目标时间：取该句起点（秒）。
///
/// 输入为**段列表 + 被点击行的 `Segment::index`**；找不到（该行已被删除或索引
/// 越界）时返回 `None`，调用方据此跳过 seek——与列表行的容错口径一致，绝不 panic。
/// 抽成纯函数以便在无 GUI 下单测（见文件末尾 `mod tests`）。
pub(crate) fn click_seek_target(segments: &[Segment], index: usize) -> Option<f64> {
    segments.iter().find(|s| s.index == index).map(|s| s.start)
}

/// 播放联动高亮判据：`current_time` 是否落在某句播放区间的**半开区间** `[start, end)` 内。
///
/// 必须与 [`crate::app::state::AppState::get_active_segment`] 同源：边界时间
/// `t == end == 下一句 start` 属于**下一句**（见该方法文档）。若误用闭区间 `<=`，
/// 相邻两句会在共享边界同时亮起「正在播放」底色，而监视器/预览只显示后一句，
/// 造成列表高亮与画面不一致。
pub(crate) fn is_playing_at(start: f64, end: f64, t: f64) -> bool {
    t >= start && t < end
}

/// 时间轴字幕轨的窗口半径：只画「视口中心 ±该秒数」内的片段。
///
/// 只在这里定义一次——渲染路径与等价性单测共用同一个来源，改窗口只改这一处。
/// 旧实现把 `15.0` 直接写死在 `render_multitrack_timeline` 的内联过滤里。
pub(crate) const TIMELINE_WINDOW_MARGIN_SECS: f64 = 15.0;

/// 片段数不超过该值时不做窗口裁剪，整条轨道全画。
///
/// 与窗口常量同理抽出来：这是渲染路径的另一半判据（句少时全画，避免用户看到
/// 「轨道上什么都没有」），单测要按同一个阈值对照旧逻辑。
pub(crate) const TIMELINE_FULL_DRAW_LIMIT: usize = 20;

/// 时间轴字幕轨这一帧要画的片段下标（**存储顺序**，即旧实现的绘制顺序）。
///
/// 判据与改造前逐条一致：片段数 ≤ [`TIMELINE_FULL_DRAW_LIMIT`] 时整表全画，
/// 否则取「与视口 ±[`TIMELINE_WINDOW_MARGIN_SECS`] 秒窗口相交」的片段，外加选中句
/// （选中句无论落在哪都必须可见，旧实现同样如此）。
///
/// 窗口查询走派生快照里的排序索引二分（见 [`timeline_visible_positions`]），
/// 不再每帧全量扫一遍片段表——几千~几万句时那是每帧一次 O(n)，播放时 40ms 一帧。
///
/// 抽成纯函数（不吃 `MainWindow`）是为了能在无 GUI 的单测里与旧逻辑逐点对照。
pub(crate) fn timeline_clip_positions(
    segments: &[Segment],
    timeline: &TimelineIndex,
    current_time: f64,
    selected: Option<usize>,
) -> Vec<usize> {
    if segments.len() <= TIMELINE_FULL_DRAW_LIMIT {
        return (0..segments.len()).collect();
    }
    timeline_visible_positions(
        segments,
        timeline,
        current_time,
        selected,
        TIMELINE_WINDOW_MARGIN_SECS,
    )
}

/// 逐词高亮里单个词的标记态：只有「已读完 / 正在读」两种可画状态。
///
/// 词级时间戳没有亚词精度（whisper token 级 offsets），无法表达词内百分比，
/// 因此当前词整词强调、已读词整词加底色；尚未开始的词不产生标记，保持默认样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WordMark {
    /// 已读完（`now >= word.end`）
    Read,
    /// 正在读（`word.start <= now < word.end`）
    Current,
}

/// 覆盖层文本里**原文行**的起始字节偏移；定位不到时返回 `None`。
///
/// 逐词高亮必须落在原文行上：译文行没有词级时间戳（双语导出是
/// `format!("{译文}\n{原文}")`，见 [`Segment::export_text`]），把词的字节区间直接
/// 叠加到整段文本上会在双语模式下整体错位。三种导出模式各自的原文行位置：
/// - [`ExportMode::RawOnly`]：整段就是原文，偏移恒为 0；
/// - [`ExportMode::TranslationOnly`]：有译文时整段是译文，没有原文行可高亮（`None`）；
///   译文为空 / 纯空白时 `export_text` 回退原文，此时 `display == raw` 按原文处理；
/// - [`ExportMode::Bilingual`]：译文为空、或译文与原文去重成单行时 `display == raw`；
///   否则原文必定是最后一行，且其前一字节是换行符。
pub(crate) fn raw_line_byte_start(
    display_text: &str,
    raw_text: &str,
    mode: ExportMode,
) -> Option<usize> {
    match mode {
        ExportMode::RawOnly => Some(0),
        ExportMode::TranslationOnly => {
            if display_text == raw_text {
                Some(0)
            } else {
                None
            }
        }
        ExportMode::Bilingual => {
            if display_text == raw_text {
                Some(0)
            } else if display_text.len() > raw_text.len()
                && display_text.ends_with(raw_text)
                && display_text.as_bytes()[display_text.len() - raw_text.len() - 1] == b'\n'
            {
                Some(display_text.len() - raw_text.len())
            } else {
                None
            }
        }
    }
}

/// 逐词高亮的核心纯函数：把 `words` 的词级时间轴映射成 `text` 上的字节区间标记。
///
/// `text` 是**原文行**（调用方先用 [`raw_line_byte_start`] 定位），返回的区间是相对
/// `text` 起点的字节偏移。行为约定：
/// - `words` 为空（旧记录 / 非 Whisper 引擎）、`text` 为空、时间非有限、段区间非法
///   （`seg_end <= seg_start`）、`now` 早于段起点 → 返回空表，渲染退回纯文本；
/// - 词文本与 `text` 顺序对不齐（用户改过错字 / 词表来自旧识别结果）→ 整体返回空表。
///   这是保守退化：宁可整句不高亮，也不能把高亮画到错位的字上。返回空表时调用方走
///   纯文本分支，天然不会产生非 char-boundary 区间；
/// - 区间按文本顺序产生且互不重叠（顺序消费 `cursor`），满足 `StyledText::with_highlights`
///   对升序、不重叠的要求；
/// - 词级时间戳没有亚词精度：第 1..N-1 个词（`now >= end`）记 [`WordMark::Read`]，
///   第 N 个词（`start <= now < end`）记 [`WordMark::Current`]，之后的词不产生区间；
///   `now` 晚于段终点时所有词自然落入「已读完」。
pub(crate) fn karaoke_word_marks(
    text: &str,
    words: &[WordTiming],
    now_sec: f64,
    seg_start: f64,
    seg_end: f64,
) -> Vec<(Range<usize>, WordMark)> {
    let mut marks = Vec::new();
    if text.is_empty()
        || words.is_empty()
        || !now_sec.is_finite()
        || !seg_start.is_finite()
        || !seg_end.is_finite()
        || seg_end <= seg_start
        || now_sec < seg_start
    {
        return marks;
    }

    // 顺序消费：每个词只从上一个词的结束位置往后找，保证区间升序且不重叠；
    // 任一环找不到即认定「文本与词表已对不齐」，整体退化为不高亮。
    let mut cursor = 0usize;
    for w in words {
        // whisper 的 token 文本可能带前导空格；比对用 trim 后的内容，
        // 但区间长度取 needle 本身（即显示文本里的真实字节数）。
        let needle = w.word.trim();
        if needle.is_empty() || !w.start.is_finite() || !w.end.is_finite() {
            continue;
        }
        let Some(rel) = text[cursor..].find(needle) else {
            return Vec::new();
        };
        let start = cursor + rel;
        let end = start + needle.len();
        cursor = end;
        // 已读完 / 正在读：零长或倒挂的词区间（end <= start）在 now 越过 end 后
        // 直接算「已读完」，不会出现「瞬时当前词」闪烁。
        if now_sec >= w.end {
            marks.push((start..end, WordMark::Read));
        } else if now_sec >= w.start {
            marks.push((start..end, WordMark::Current));
        }
    }
    marks
}

/// 词级高亮的视觉样式：当前词换强调色 + 加粗；已读词只加浅色底。
///
/// 样式全部取自 [`Theme`]，不硬编码颜色，浅色 / 深色主题自动跟随：
/// - 已读词用 `background_color: Theme::tint_mint_soft()`（与编辑器「播放中」胶囊
///   同源，见 `render_video_monitor` 标头）。不用「弱化字色」的理由：覆盖层字色与
///   字号由用户的字幕样式配置决定（`subtitle_computed_colors`），调暗前景色会和用户
///   配色打架；`HighlightStyle::fade_out` 在亮画面 / 浅色主题下还可能把字淡到看不清。
///   底色方案只动底色，用户字色、字号原样保留。
/// - 当前词用 `color: Theme::accent_mint()` + `FontWeight::BOLD`。mint 在本应用里就是
///   「播放 / 进度」语义色（进度条、时间轴激活块、「播放中」胶囊），无需为此引入新
///   模式色；加粗让「当前词」在浅底已读词的包围下依然突出。
pub(crate) fn karaoke_mark_style(mark: WordMark) -> HighlightStyle {
    match mark {
        WordMark::Current => HighlightStyle {
            color: Some(Theme::accent_mint().into()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
        WordMark::Read => HighlightStyle {
            background_color: Some(Theme::tint_mint_soft().into()),
            ..Default::default()
        },
    }
}

/// 监视器「视口内屏」（画面可用的那块黑底区域）的像素尺寸估算。
///
/// 由窗口视口尺寸、布局模式（左右二分 / 上下堆叠）与右侧面板宽度推出，用于把画面按
/// 真实宽高比等比内接——不再依赖 taffy 的 aspect-ratio 推导，竖屏/方屏视频也不会溢出。
fn monitor_screen_size(
    viewport_w: f32,
    viewport_h: f32,
    is_processing: bool,
    stacked: bool,
    right_w: f32,
) -> (f32, f32) {
    // 视口内屏 = 监视器外框去掉标头与控制条
    let chrome_v = Theme::HEADER_H_SM + Theme::CONTROL_BAR_H;
    let screen_h = if stacked {
        EDITOR_STACK_MONITOR_H - chrome_v
    } else {
        let editor_h =
            viewport_h - Theme::TITLEBAR_H - if is_processing { Theme::BANNER_H } else { 0.0 };
        editor_h - Theme::TIMELINE_H - chrome_v
    };
    let screen_w = if stacked {
        viewport_w
    } else {
        viewport_w - right_w - EDITOR_SPLIT_DIVIDER_W
    };
    (screen_w.max(0.0), screen_h.max(0.0))
}

/// 上下并列字幕行高度（双语模式：时间 + 原文 + 译文）
const SUBTITLE_ROW_H_BILINGUAL: f32 = 82.0;
/// 上下并列字幕行高度（单语模式：时间 + 原文）
const SUBTITLE_ROW_H_MONO: f32 = 56.0;

/// 说话人标签的配色：4 个说话人各占一色，超过则回落到第一色循环。
/// 只在标签本身着色（不染整行），避免与「选中行」的高亮底色互相干扰。
fn speaker_color(speaker: u32) -> gpui::Rgba {
    match speaker % 4 {
        0 => Theme::accent_mint(),
        1 => Theme::accent_blue(),
        2 => Theme::accent_orange(),
        _ => Theme::accent_primary(),
    }
}

fn speaker_tint(speaker: u32) -> gpui::Rgba {
    match speaker % 4 {
        0 => Theme::tint_mint_soft(),
        1 => Theme::tint_blue_soft(),
        2 => Theme::tint_red_soft(),
        _ => Theme::tint_primary_soft(),
    }
}

fn speaker_border(speaker: u32) -> gpui::Rgba {
    match speaker % 4 {
        0 => Theme::tint_mint_border(),
        1 => Theme::tint_blue_border(),
        2 => Theme::tint_red_border(),
        _ => Theme::tint_primary_border(),
    }
}

/// 把磁盘上的帧文件解码成 GPUI 可直接上传纹理的 `RenderImage`。
///
/// 为什么不让 GPUI 自己按路径加载（`img(路径)`）见 `render_monitor_picture`
/// 的分支说明：那条路每张图都会永久留在 `App::loading_assets` 里，拖动时间轴
/// 每 40ms 就多一张，几百 MB 起。这里由抽帧线程顺手解码，解码结果只保留当前帧。
///
/// **必须跑在后台线程**：960 宽的 JPG 解一次十几毫秒，放渲染线程就是稳定掉帧。
///
/// 字节序固定 BGRA：GPUI 的图集纹理格式是 `B8G8R8A8_UNORM`（见 gpui
/// `platform/windows/directx_atlas.rs`），而 `image` crate 给的是 RGBA，
/// 两者 R/B 相反——与内嵌播放器同源（`video_player` 里 ffmpeg 直出 bgra）。
pub(crate) fn render_image_from_frame_file(path: &Path) -> Option<Arc<RenderImage>> {
    let decoded = image::open(path).ok()?;
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut bytes = rgba.into_raw();
    // 就地交换 R/B 两个通道（image 给的是 RGBA，GPUI 要 BGRA）
    let mut offset = 0;
    while offset + 2 < bytes.len() {
        bytes.swap(offset, offset + 2);
        offset += 4;
    }
    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, bytes)?;
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(
        Frame::new(buffer),
        1,
    ))))
}
/// 监视器画面来源判定。抽成纯函数是为了把「同版本第二次渲染不得掉回静态帧/
/// 占位」这条语义脱离 GUI 单测钉住（见文件末尾 `mod tests`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MonitorPictureSource {
    /// 播放器有画面，且本帧取到了未消费的新版本帧 → 重建 RenderImage
    LiveNewFrame,
    /// 播放器有画面，但本版本已消费过 → 复用已构建的 RenderImage（不得重建、不得掉静态帧）
    LiveReuse,
    /// 播放器有画面，但既没有新帧也没有可用纹理 → 留空等下一帧（不得掉静态帧）
    LivePending,
    /// 播放器无画面（未播放 / `stop()` 之后）→ 静态时间轴帧 / 占位链路
    Static,
}

pub(crate) fn monitor_picture_source(
    has_live_picture: bool,
    has_new_frame: bool,
    has_cached_image: bool,
) -> MonitorPictureSource {
    if !has_live_picture {
        return MonitorPictureSource::Static;
    }
    match (has_new_frame, has_cached_image) {
        (true, _) => MonitorPictureSource::LiveNewFrame,
        (false, true) => MonitorPictureSource::LiveReuse,
        (false, false) => MonitorPictureSource::LivePending,
    }
}

/// 「句间最小间隔」界面给出的三档间隔（秒）：以 [`crate::subtitle::DEFAULT_MIN_GAP_SECS`]
/// 为基准逐档加倍（0.04 / 0.08 / 0.16）。取默认值当第一档，用户不用去猜「官方推荐」是多少。
const MIN_GAP_PRESETS: [f64; 3] = [
    crate::subtitle::DEFAULT_MIN_GAP_SECS,
    crate::subtitle::DEFAULT_MIN_GAP_SECS * 2.0,
    crate::subtitle::DEFAULT_MIN_GAP_SECS * 4.0,
];

/// 句间最小间隔档位在 `retime_op` 里的起始值：`4..=6` 依次对应
/// [`MIN_GAP_PRESETS`]，`0..=3` 仍是平移 / 缩放 / 铺满——两者共用同一个
/// 「当前方式」选择位（本文件之外不新增窗口状态字段）。
const MIN_GAP_OP_BASE: u8 = 4;

/// 「句间最小间隔」那一行的小字说明（只此一处，避免两处文案漂移）。
const MIN_GAP_INTRO: &str = "上一句还没消失下一句就开始读时，用它把间隔推开";

/// 「句间最小间隔」的预览 / 回执文案（纯函数、无 GUI，便于单测）。
///
/// `applied == false` 是点「应用间隔」**之前**的预览（「将改动 N 句」）；
/// `applied == true` 是写回后的回执（「已推开 N 句」）。无命中时两种口径都明说
/// 「无需改动」，用户不会对着一个没有反应的按钮反复点。
///
/// # 为什么 `shortened` 也要看
///
/// `changed` 只数**起点被抬**的句；「收短前一句、让后一句保住原终点」那条路径只加
/// `shortened`——只看 `changed` 会把这种真实改动说成「无需改动」。
fn min_gap_hint(plan: &GapPlan, applied: bool) -> String {
    if plan.changed == 0 && plan.shortened == 0 {
        return "间隔已经够宽，无需改动".to_string();
    }
    let mut text = if plan.changed > 0 {
        format!(
            "{} {} 句",
            if applied { "已推开" } else { "将改动" },
            plan.changed
        )
    } else {
        format!(
            "{} {} 句的结束时间",
            if applied { "已收短" } else { "将收短" },
            plan.shortened
        )
    };
    if plan.changed > 0 && plan.shortened > 0 {
        text.push_str(&format!("，其中 {} 句收短前句", plan.shortened));
    }
    if plan.unresolved {
        text.push_str("（个别短句仍贴太紧）");
    }
    text
}

impl MainWindow {
    /// 渲染类似剪映 / Premiere 风格的剪辑工作区布局 (左中右之 中：视频与时间轴，右：字幕属性)
    pub(crate) fn render_editor_layout(
        &mut self,
        viewport_w: f32,
        viewport_h: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        // 布局随窗口宽度二选一，见 `EDITOR_STACK_BELOW_W` 的说明
        let stacked = viewport_w < EDITOR_STACK_BELOW_W;
        // 右侧字幕面板宽度：与下面的拖动逻辑同源，这里先算一次，供监视器尺寸估算复用
        let right_w = self
            .editor_split_w
            .unwrap_or(EDITOR_RIGHT_PANEL_W)
            .clamp(360.0, (viewport_w * 0.5).max(360.0));
        // 监视器视口内屏尺寸（画面等比内接的可用空间）
        let monitor_screen =
            monitor_screen_size(viewport_w, viewport_h, is_processing, stacked, right_w);

        div()
            .id("editor-workspace-layout")
            .flex()
            .flex_col()
            .flex_1()
            .w_full()
            .h_full()
            .overflow_hidden()
            .child(if is_processing {
                div()
                    .w_full()
                    .h(px(Theme::BANNER_H))
                    .px(px(Theme::PAGE_PAD))
                    .bg(Theme::tint_mint_soft())
                    .border_b_1()
                    .border_color(Theme::tint_mint_border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(primitives::stat_dot_sm(Theme::accent_mint()))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::accent_mint())
                                    .child("语音转写进行中，完成后自动同步到剪辑工作台"),
                            ),
                    )
                    .child(
                        // 标头小胶囊走 primitives::btn_sm_outline
                        primitives::btn_sm_outline("查看进度")
                            .id("switch-to-generator-banner-btn")
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .hover(|s| s.bg(Theme::bg_hover()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Generate;
                                cx.notify();
                            })),
                    )
            } else {
                div()
            })
            .child(if stacked {
                // 窄窗口降级：视频监视器在上、字幕配置在下，两列各占整行宽度
                div()
                    .id("editor-main-stacked")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(
                        div()
                            .w_full()
                            .flex_none()
                            .h(px(EDITOR_STACK_MONITOR_H))
                            .border_b_1()
                            .border_color(Theme::border())
                            .overflow_hidden()
                            .child(self.render_video_monitor(monitor_screen, window, cx)),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.render_subtitle_inspector(true, cx)),
                    )
            } else {
                let is_dragging = self.editor_split_drag.is_some();

                div()
                    .id("editor-main-split")
                    .flex()
                    .flex_row()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if event.pressed_button != Some(MouseButton::Left) {
                            return;
                        }
                        if let Some((start_w, start_x)) = this.editor_split_drag {
                            let max_w = (viewport_w * 0.5).max(360.0);
                            let dx = f32::from(event.position.x) - start_x;
                            let new_w = (start_w - dx).clamp(360.0, max_w);
                            this.editor_split_w = Some(new_w);
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.editor_split_drag = None;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.editor_split_drag = None;
                            cx.notify();
                        }),
                    )
                    // 左侧：视频监视器 (占主要空间，类似 B 站 / YouTube 播放页)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .min_w(px(EDITOR_LEFT_MIN_W))
                            .overflow_hidden()
                            .child(self.render_video_monitor(monitor_screen, window, cx)),
                    )
                    // 中间：可自由拖动的分隔条（宽度不超过窗口的 50%）
                    .child(
                        div()
                            .id("editor-split-divider")
                            .w(px(5.0))
                            .h_full()
                            .bg(if is_dragging {
                                Theme::accent_mint()
                            } else {
                                Theme::border()
                            })
                            .cursor_col_resize()
                            .hover(|s| s.bg(Theme::accent_mint()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.editor_split_drag =
                                        Some((right_w, f32::from(event.position.x)));
                                    cx.notify();
                                }),
                            ),
                    )
                    // 右侧：字幕配置与列表区域 (紧凑侧边面板，可拖动调节宽度，上限 50%)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .w(px(right_w))
                            .h_full()
                            .overflow_hidden()
                            .child(self.render_subtitle_inspector(false, cx)),
                    )
            })
            // 底部：时间轴 (全宽横跨)
            .child(self.render_multitrack_timeline(cx))
    }

    /// 渲染专业视频监视器 (Preview Monitor)
    pub(crate) fn render_video_monitor(
        &mut self,
        screen: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cur_time = self.state.current_time;
        let tot_time = self.state.total_duration.max(1.0);
        // 命中句定位走 MainWindow 侧的缓存（上一帧命中的下标 + 半开区间判据）：
        // 播放时每 40ms 重绘一帧，对几千句逐帧线性扫描纯属浪费；只有跨句的那一帧
        // 才回落 `AppState::get_active_segment()` 重新定位（见
        // `cached_active_segment_index`）。这里仍然克隆整条 `Segment`：下面
        // `render_monitor_picture` 要用可变借用 `self`，这个引用活不到那里。
        let active_seg = self
            .cached_active_segment_index()
            .and_then(|idx| self.state.segments.get(idx).cloned());
        let is_playing = self.state.is_playing;

        div()
            .id("editor-video-monitor")
            .w_full()
            .flex_1()
            .min_h(px(Theme::MONITOR_MIN_H))
            .bg(Theme::bg_media())
            .flex()
            .flex_col()
            .overflow_hidden()
            // 监视器标头
            .child(
                div()
                    .h(px(Theme::HEADER_H_SM))
                    .px(px(Theme::PAGE_PAD))
                    .bg(Theme::bg_sidebar())
                    .border_b_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY_LG))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_primary())
                                    .child("视频预览"),
                            )
                            .child(if is_playing {
                                // 瞬时状态胶囊走 primitives::tag_tinted
                                primitives::tag_tinted(
                                    "播放中",
                                    Theme::tint_mint_soft(),
                                    Theme::tint_mint_border(),
                                    Theme::accent_mint(),
                                )
                            } else {
                                div()
                            }),
                    )
                    .child(
                        // 标头小胶囊走 primitives::btn_sm_outline
                        primitives::btn_sm_outline("独立窗口")
                            .id("monitor-ffplay-btn")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.play_video(cx);
                            })),
                    ),
            )
            // 监视器视口屏幕：视频画面按真实宽高比等比适配居中（分辨率探测前回退 16:9）
            .child(
                div()
                    .id("monitor-viewport-screen")
                    .flex_1()
                    .w_full()
                    .min_h(px(Theme::VIEWPORT_MIN_H))
                    .relative()
                    .bg(Theme::bg_media_deep())
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    // 画面容器 (stage)：与视频画面等比，是字幕覆盖层的定位基准，
                    // 保证字幕始终压在画面上而不是视口黑边里
                    .child({
                        let video_aspect = self.state.video_aspect();
                        // 画面尺寸由「视口内屏尺寸 + 视频宽高比」显式算出并双向内接，
                        // 而不是靠 taffy 的 aspect-ratio 推导：后者在竖屏/方屏下宽度不会
                        // 随高度收缩，画面会溢出视口。显式尺寸下「监视器里看到多大，
                        // 字幕覆盖层的定位基准就是多大」，两者永远一致。
                        let (screen_w, screen_h) = screen;
                        let (fit_w, fit_h) = fit_video_size(
                            video_aspect,
                            screen_w * MONITOR_VIDEO_FIT_FRAC,
                            screen_h * MONITOR_VIDEO_FIT_FRAC,
                        );
                        let mut stage = div().relative();
                        if fit_w > 0.0 && fit_h > 0.0 {
                            stage = stage.w(px(fit_w)).h(px(fit_h));
                        } else {
                            // 尺寸不可得（视口尚未测量 / 比例非法）时退回旧的自适应行为
                            stage = stage.w_full().max_h_full();
                            stage.style().aspect_ratio = Some(video_aspect);
                        }
                        stage
                            // 视频画面展示 (实时内嵌播放 vs 静态时间轴帧)
                            .child(self.render_monitor_picture(window, cx))
                            // 字幕覆盖层：完全由 config.subtitle_style 驱动，
                            // 主界面改样式即时反映在这里（预览按 PREVIEW_SCALE 缩放显示）
                            .child({
                                let style = self.state.config.subtitle_style.clone();
                                let font_px = style.preview_font_px();
                                let (fg, bg, border) = subtitle_computed_colors(&style);
                                // 宽度与编辑卡里的预览框同源：拖把手时两者在同一帧一起变，
                                // 不存在「预览改了、画面上没跟上」的延迟
                                let box_w = self.subtitle_box_w();
                                let export_mode = self.editor_export_mode;
                                div()
                                    .absolute()
                                    .bottom(relative(style.preview_bottom_ratio()))
                                    .left_0()
                                    .right_0()
                                    .flex()
                                    .justify_center()
                                    .px_6()
                                    .child(if let Some(seg) = active_seg {
                                        div()
                                            // 固定宽度（不是 max_w）：拖动把手时画面上的
                                            // 字幕框要跟着同宽变化，而不是只有超长句才受影响。
                                            // 短句在框内居中，长句按此宽度折行——与编辑卡的
                                            // 预览框是同一个宽度值，两者同步增减。
                                            .w(px(box_w))
                                            .min_w_0()
                                            .px_3()
                                            .py_1()
                                            .rounded_md()
                                            .bg(bg)
                                            .border_1()
                                            .border_color(border)
                                            .text_size(px(font_px))
                                            .font_weight(if style.is_bold {
                                                FontWeight::BOLD
                                            } else {
                                                FontWeight::NORMAL
                                            })
                                            .text_color(fg)
                                            .text_align(match style.alignment.as_str() {
                                                "left" => TextAlign::Left,
                                                "right" => TextAlign::Right,
                                                _ => TextAlign::Center,
                                            })
                                            .line_height(px(font_px * style.line_spacing))
                                            // 预览与导出同源：按当前导出内容模式渲染，
                                            // 译好之后在监视器里就能直接看到译文/双语。
                                            // 有词级时间戳时，原文行做「卡拉 OK」逐词高亮：已读词加薄荷浅底、
                                            // 当前词换强调色加粗（样式见 `karaoke_mark_style`）；双语模式下译文行
                                            // 没有词级时间，只高亮原文行。
                                            .child({
                                                let display_text = seg.export_text(export_mode);
                                                let raw_text = seg.display_text();
                                                let highlights = raw_line_byte_start(
                                                    &display_text,
                                                    raw_text,
                                                    export_mode,
                                                )
                                                .map(
                                                    |base| -> Vec<(Range<usize>, HighlightStyle)> {
                                                        karaoke_word_marks(
                                                            raw_text, &seg.words, cur_time,
                                                            seg.start, seg.end,
                                                        )
                                                        .into_iter()
                                                        .map(|(range, mark)| {
                                                            (
                                                                base + range.start
                                                                    ..base + range.end,
                                                                karaoke_mark_style(mark),
                                                            )
                                                        })
                                                        .collect()
                                                    },
                                                )
                                                .unwrap_or_default();
                                                if highlights.is_empty() {
                                                    // 无词级信息 / 文本与词表被改到对不齐：退回纯文本，外观与改动前一致。
                                                    display_text.into_any_element()
                                                } else {
                                                    StyledText::new(display_text)
                                                        .with_highlights(highlights)
                                                        .into_any_element()
                                                }
                                            })
                                    } else {
                                        div()
                                    })
                            })
                    }),
            )
            // 监视器底部播放控制器与时间码显示
            .child(
                div()
                    .h(px(Theme::CONTROL_BAR_H))
                    .px(px(Theme::PAGE_PAD))
                    .bg(Theme::bg_sidebar())
                    .border_t_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    // 左侧占位 (保证正中间对齐)
                    .child(div().flex_1().flex().items_center().justify_start())
                    // 中间：iOS 紧凑媒体控制条 (居中 + 放大主要播放按钮)
                    .child(
                        div().flex().items_center().justify_center().child(
                            div()
                                .bg(Theme::bg_input())
                                .p(px(Theme::CONTROL_INSET))
                                .rounded_full()
                                .border_1()
                                .border_color(Theme::border_mid())
                                .flex()
                                .items_center()
                                .gap(px(Theme::CTRL_GAP_TIGHT))
                                // 上一句
                                .child(primitives::pill_btn("上句").id("ctrl-prev-seg").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.jump_prev_segment(cx);
                                    }),
                                ))
                                // 快退 1 秒
                                .child(primitives::pill_btn("-1s").id("ctrl-step-back").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.halt_preview_playback();
                                        let target = (this.state.current_time - 1.0).max(0.0);
                                        this.state.seek_to(target);
                                        this.trigger_extract_frame(cx);
                                        cx.notify();
                                    }),
                                ))
                                // 实时播放 / 暂停 (高亮突出放大按钮)
                                .child(
                                    primitives::pill_btn_solid(
                                        if is_playing { "暂停" } else { "播放" },
                                        if is_playing {
                                            Theme::accent_orange()
                                        } else {
                                            Theme::accent_mint()
                                        },
                                    )
                                    .id("ctrl-play-pause")
                                    .px(px(Theme::SPACE_6))
                                    .text_size(px(Theme::TEXT_BODY_LG))
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.toggle_play_preview(cx);
                                        },
                                    )),
                                )
                                // 快进 1 秒
                                .child(primitives::pill_btn("+1s").id("ctrl-step-fwd").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.halt_preview_playback();
                                        let target = this.state.current_time + 1.0;
                                        this.state.seek_to(target);
                                        this.trigger_extract_frame(cx);
                                        cx.notify();
                                    }),
                                ))
                                // 下一句
                                .child(primitives::pill_btn("下句").id("ctrl-next-seg").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.jump_next_segment(cx);
                                    }),
                                )),
                        ),
                    )
                    // 右侧时间码 (iOS 胶囊卡片，右对齐)
                    .child(
                        div().flex_1().flex().items_center().justify_end().child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_full()
                                .bg(Theme::bg_input())
                                .border_1()
                                .border_color(Theme::border_mid())
                                .flex()
                                .items_center()
                                .gap_1()
                                .font_family("Consolas")
                                .text_size(px(Theme::TEXT_SMALL))
                                .child(
                                    div()
                                        .text_color(Theme::accent_mint())
                                        .font_weight(FontWeight::BOLD)
                                        .child(seconds_to_hms(cur_time)),
                                )
                                .child(div().text_color(Theme::text_muted()).child("/"))
                                .child(
                                    div()
                                        .text_color(Theme::text_secondary())
                                        .child(seconds_to_hms(tot_time)),
                                ),
                        ),
                    ),
            )
    }

    /// 监视器画面内容：实时内嵌播放帧 vs 静态时间轴帧 vs 占位提示。
    /// 帧尺寸随视频真实宽高比动态变化，纹理构建与缓存均以本帧自带尺寸为准。
    fn render_monitor_picture(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        // 「当前有画面」由播放器侧维护（发布帧置 true、stop() 置 false、暂停保留定格帧）：
        // 帧被 `take_unconsumed_frame` 取走后槽位本来就是空的，旧的
        // `get_frame().is_some()` 判据会把「画面已交给 UI 显示」误判成「没有画面」。
        let has_live_picture = self.state.video_player.has_live_picture();
        // 只有未消费的新版本帧才会被取走（槽位里的 Arc 直接移交，不 clone 数据）。
        // 同一版本的第二次渲染拿到 None——此时复用 `cached_live_image` 里的纹理，
        // 绝不能因为「没取到帧」掉回静态帧/占位（那会闪）。
        let live_frame = self.state.video_player.take_unconsumed_frame();
        let source = monitor_picture_source(
            has_live_picture,
            live_frame.is_some(),
            self.cached_live_image.is_some(),
        );

        if source != MonitorPictureSource::Static {
            // GPUI RenderImage 底层纹理严格要求 BGRA 格式（wgpu::TextureFormat::Bgra8Unorm），FFmpeg 已按 bgra 直出。
            // 画面来源只由 `monitor_picture_source` 决定：新版本帧重建纹理、同版本复用缓存。
            let render_img: Option<Arc<RenderImage>> = match (source, live_frame) {
                (MonitorPictureSource::LiveNewFrame, Some((version, frame))) => {
                    let (frame_w, frame_h) = (frame.width, frame.height);
                    // 帧已被 take 成独占 Arc，`try_unwrap` 必然成功——这里就是每帧
                    // 3.7MB 深拷贝的消失点；clone 兜底只防异常路径 panic。
                    let data = match Arc::try_unwrap(frame) {
                        Ok(f) => f.data,
                        Err(arc) => arc.data.clone(),
                    };
                    match ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(frame_w, frame_h, data) {
                        Some(buffer) => {
                            let new_img = Arc::new(RenderImage::new(SmallVec::from_elem(
                                Frame::new(buffer),
                                1,
                            )));
                            // 换帧时把上一张显式交还给 GPUI：精灵图集只按 image_id 回收
                            // （`Window::drop_image` → `Direct3DAtlas::remove`，见 gpui 0.2.2
                            // window.rs 与 platform/windows/directx_atlas.rs）。不回收，
                            // 就等于每渲染一帧都留一张常驻 GPU 纹理：播放几分钟显存见底，
                            // 之后一路掉帧——「越看越卡」就是这么来的。
                            if let Some((_, _, _, old)) = self.cached_live_image.take() {
                                cx.drop_image(old, Some(window));
                            }
                            self.cached_live_image =
                                Some((version, frame_w, frame_h, new_img.clone()));
                            Some(new_img)
                        }
                        None => None,
                    }
                }
                // 同版本第二次渲染：复用已构建的纹理。(版本, 宽, 高) 判据由 take 协议
                // 保证——播放器只在版本前进时交出帧，能进这里缓存就是当前版本的图。
                (MonitorPictureSource::LiveReuse, _) => self
                    .cached_live_image
                    .as_ref()
                    .map(|(_, _, _, cached_img)| cached_img.clone()),
                // 有画面但这帧既没有新帧也没有缓存纹理（首帧未到 / 缓存缺失）：留空等
                // 下一帧，同样不得掉回静态帧分支。
                _ => None,
            };
            if let Some(render_img) = render_img {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(img(render_img).size_full().object_fit(ObjectFit::Contain))
            } else {
                div().size_full()
            }
        } else if let Some(frame_img) = self.state.preview_frame_image.clone() {
            // 时间轴定位帧走**我们自己解码**的 RenderImage，而不是 `img(路径)`。
            //
            // `img(路径)` 那条路有两个只涨不跌的口子（都读自 gpui 0.2.2 源码）：
            //   1) 解码结果按「资源哈希」塞进 `App::loading_assets` 后永不回收
            //      （`App::fetch_asset` 只增不删），一张 960 宽的帧 ≈ 2MB RGBA；
            //   2) GPU 侧在精灵图集里留一张只按 image_id 回收的纹理，没人调
            //      `drop_image` 就永远不释放。
            // 拖动时间轴是「每 40ms 一个新路径」，于是内存与显存随时间线性上涨，
            // 用久了就是卡。这里只持有**当前这一张**，换帧时把上一张交还回收。
            if !matches!(
                self.cached_static_image.as_ref(),
                Some(old) if Arc::ptr_eq(old, &frame_img)
            ) {
                if let Some(old) = self.cached_static_image.replace(frame_img.clone()) {
                    cx.drop_image(old, Some(window));
                }
            }
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(ImageSource::Render(frame_img))
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
        } else if let Some(ref frame_path) = self.state.preview_frame_path {
            // 解码失败时的兜底：**不再**把路径交给 GPUI 的资源加载器。
            // `img(路径)` 会在 `App::loading_assets` 里按资源哈希永久留一张解码结果
            // （只增不删，见上面的分支说明），而拖动时间轴是「每 0.1s 一个新路径」，
            // 于是内存随时间线性上涨——这条兜底路正是那个口子最后一个入口。
            // 这里改用同一个解码入口再试一次，成败都按路径记进
            // `cached_fallback_frame`，同一路径不会逐帧重解（兜底跑在渲染线程上，
            // 每帧重试一次失败解码同样是白付 IO 与解码头）。解不出来就显示同步
            // 占位，等下一帧解码成功自然回到上面的分支。
            let fallback_image = match self.cached_fallback_frame.as_ref() {
                Some((path, decoded)) if path == frame_path => decoded.clone(),
                _ => {
                    let decoded = render_image_from_frame_file(frame_path);
                    self.cached_fallback_frame = Some((frame_path.clone(), decoded.clone()));
                    decoded
                }
            };
            if let Some(fallback_image) = fallback_image {
                // 与上面那条分支共用同一个「只留一张」的回收槽：GPUI 的精灵图集
                // 只认 `drop_image`，换图时必须把上一张显式交还。
                if !matches!(
                    self.cached_static_image.as_ref(),
                    Some(old) if Arc::ptr_eq(old, &fallback_image)
                ) {
                    if let Some(old) = self.cached_static_image.replace(fallback_image.clone()) {
                        cx.drop_image(old, Some(window));
                    }
                }
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        img(ImageSource::Render(fallback_image))
                            .size_full()
                            .object_fit(ObjectFit::Contain),
                    )
            } else {
                div().flex().flex_col().items_center().gap_1p5().child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(Theme::accent_on_media())
                        .child("正在同步视频画面..."),
                )
            }
        } else if self.state.selected_file.is_some() {
            div().flex().flex_col().items_center().gap_1p5().child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::accent_on_media())
                    .child("正在同步视频画面..."),
            )
        } else {
            div().flex().flex_col().items_center().gap_1p5().child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    // 监视器视口在任何主题下都是深底，空态提示也在深底上，
                    // 必须用媒体区 token；用 text_muted 会在浅色主题下变成深字压深底
                    .text_color(Theme::text_on_media())
                    .child("拖动时间轴或点击播放，画面将在此实时呈现"),
            )
        }
    }

    /// 渲染字幕属性检查器与错别字编辑区 (Inspector)
    /// 渲染字幕属性检查器与多语言配置表格 (Inspector & Table)
    pub(crate) fn render_subtitle_inspector(
        &mut self,
        stacked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 搜索命中的行下标按需重算（键不变时零成本复用），虚拟列表据此渲染
        self.refresh_subtitle_filter();
        let sel_idx = self.state.selected_segment_index;
        let cur_seg = sel_idx
            .and_then(|idx| self.state.segments.iter().find(|s| s.index == idx))
            .cloned();
        let cur_text = self.state.editing_text.clone();
        let panel = self.subtitle_panel;

        div()
            .id("editor-subtitle-inspector")
            .flex_1()
            // 宽度约束交给外层列容器（见 `EDITOR_RIGHT_MIN_W`）；这里显式允许收缩，
            // 因为列内每一行都已按「先收缩、再折行」处理，不需要再撑住一个固定下限。
            .min_w_0()
            .h_full()
            .bg(Theme::bg_sidebar())
            .flex()
            .flex_col()
            .overflow_hidden()
            // ── 顶部标头栏 ──
            .child(
                div()
                    .h(px(Theme::HEADER_H))
                    .px(px(Theme::PAGE_PAD))
                    .bg(Theme::bg_sidebar())
                    .border_b_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        primitives::panel_title("字幕面板"),
                    )
                    .child(
                        // 两张面板互斥切换：顶部一对分段选项代替原先的「展开 / 收起」开关。
                        // 面板高度有限，样式卡与翻译卡纵向堆叠时必须滚动才能看到下面那张，
                        // 拆成互斥视图后一次只占一份版面。
                        primitives::segmented_cluster()
                            // 样式面板：全局字幕样式与排版
                            .child(
                                primitives::segmented(
                                    "字幕样式",
                                    panel == EditorSubtitlePanel::Style,
                                    false,
                                )
                                .id("btn-panel-subtitle-style")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.subtitle_panel = EditorSubtitlePanel::Style;
                                    cx.notify();
                                })),
                            )
                            // 翻译面板：多语言翻译与进度
                            .child(
                                primitives::segmented(
                                    "字幕翻译",
                                    panel == EditorSubtitlePanel::Translate,
                                    false,
                                )
                                .id("btn-panel-subtitle-translate")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.subtitle_panel = EditorSubtitlePanel::Translate;
                                    cx.notify();
                                })),
                            )
                            // 统计与导出面板：整篇的时长 / 字数 / 语速 / 过长句，
                            // 以及导出内容 / 文件名模板 / 格式选择。这些数字此前无处可看
                            // （只能自己数），而 CPS 与「过长句」正是字幕交付会被打回的硬指标；
                            // 导出又正是这份字幕最后的交付动作，两者合并同屏最贴合。
                            .child(
                                primitives::segmented(
                                    "统计与导出",
                                    panel == EditorSubtitlePanel::Stats,
                                    false,
                                )
                                .id("btn-panel-subtitle-stats")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.subtitle_panel = EditorSubtitlePanel::Stats;
                                    cx.notify();
                                })),
                            ),
                    ),
            )
            // ── 属性面板主体 ──
            .child(
                div()
                    .id("editor-subtitle-inspector-body")
                    .flex_1()
                    .min_h_0()
                    // 样式卡 + 单句编辑卡 + 翻译卡 + 对照表的自然高度≈830px，而本面板
                    // 在 869px 高的窗口里只剩 ~590px。此前左右二分模式用的是
                    // `overflow_hidden`，超出部分既不滚动也不被压缩，结果「字幕翻译」卡
                    // 被挤成 0 高、正文整段消失（只剩下方对照表留在裁切区外）。这里两种
                    // 布局一律放开纵向滚动，并把各卡片设成 `flex_none()`（不参与收缩），
                    // 卡片保持自然高度、由用户滚动查看。
                    .overflow_y_scroll()
                    .p(px(Theme::CARD_PAD_SM))
                    .flex()
                    .flex_col()
                    .gap(px(Theme::CARD_GAP))
                    // 1. 全局字幕样式与排版配置卡片（仅「字幕样式」面板可见）
                    .child(
                        if panel == EditorSubtitlePanel::Style {
                            let cur_style = self.state.config.subtitle_style.clone();

                            primitives::card_sm()
                                .flex_none()
                                .gap(px(Theme::SPACE_2))
                                // 卡片标题与恢复默认
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(Theme::TEXT_BODY))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(Theme::text_muted())
                                                .child("全局字幕样式与排版"),
                                        )
                                        .child(
                                            div()
                                                .id("btn-reset-subtitle-style")
                                                .px_2()
                                                .py_0p5()
                                                .rounded_md()
                                                .bg(Theme::bg_inset())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.config.subtitle_style = crate::utils::SubtitleStyleConfig::default();
                                                    this.state.save_subtitle_style();
                                                    cx.notify();
                                                }))
                                                .child("恢复默认"),
                                        ),
                                )
                                // 1) 文字颜色池（纯色圆点 + 最后一个调色板色轮）
                                .child({
                                    let is_custom_color = !crate::utils::SUBTITLE_COLOR_PALETTE
                                        .iter()
                                        .any(|(_, hex)| cur_style.primary_color.eq_ignore_ascii_case(hex));
                                    let is_picker_open = self.custom_color_picker_open;

                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(
                                                    div()
                                                        .w(px(58.0))
                                                        .flex_shrink_0()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("文字颜色"),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .flex_wrap()
                                                        .gap_2()
                                                        // 7 个预设纯色小圆点
                                                        .children(crate::utils::SUBTITLE_COLOR_PALETTE.into_iter().enumerate().map(|(idx, (_name, hex))| {
                                                            let is_sel = cur_style.primary_color.eq_ignore_ascii_case(hex);
                                                            let (r, g, b) = crate::utils::SubtitleStyleConfig::hex_to_rgb(hex);
                                                            let swatch_color = rgb((r as u32) << 16 | (g as u32) << 8 | (b as u32));
                                                            div()
                                                                .id(("color-opt", idx))
                                                                .w(px(22.0))
                                                                .h(px(22.0))
                                                                .rounded_full()
                                                                .cursor_pointer()
                                                                .bg(swatch_color)
                                                                .border_2()
                                                                .border_color(if is_sel {
                                                                    Theme::accent_mint()
                                                                } else {
                                                                    rgba(0x80808044)
                                                                })
                                                                .hover(|s| if !is_sel { s.border_color(Theme::text_secondary()) } else { s })
                                                                .on_click(cx.listener({
                                                                    let hex_str = hex.to_string();
                                                                    move |this, _, _, cx| {
                                                                        this.state.config.subtitle_style.primary_color = hex_str.clone();
                                                                        this.state.save_subtitle_style();
                                                                        cx.notify();
                                                                    }
                                                                }))
                                                        }))
                                                        // 最后一个：调色板色轮圆点（可滑动自定义调色）
                                                        .child({
                                                            let is_wheel_active = is_custom_color || is_picker_open;
                                                            let (r, g, b) = crate::utils::SubtitleStyleConfig::hex_to_rgb(&cur_style.primary_color);
                                                            let cur_swatch = rgb((r as u32) << 16 | (g as u32) << 8 | (b as u32));
                                                            div()
                                                                .id("color-wheel-toggle-btn")
                                                                .w(px(22.0))
                                                                .h(px(22.0))
                                                                .rounded_full()
                                                                .cursor_pointer()
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(if is_custom_color { cur_swatch } else { Theme::bg_inset() })
                                                                .border_2()
                                                                .border_color(if is_wheel_active {
                                                                    Theme::accent_mint()
                                                                } else {
                                                                    rgba(0x80808044)
                                                                })
                                                                .hover(|s| s.border_color(Theme::accent_mint()))
                                                                .child(
                                                                    if is_custom_color {
                                                                        div()
                                                                    } else {
                                                                        div()
                                                                            .text_size(px(10.0))
                                                                            .child("🎨")
                                                                    }
                                                                )
                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                    this.custom_color_picker_open = !this.custom_color_picker_open;
                                                                    cx.notify();
                                                                }))
                                                        })
                                                )
                                        )
                                        // 若调色板展开：显示环形色轮与自由滑动调色卡片
                                        .child(if is_picker_open {
                                            self.render_color_wheel_picker(&cur_style.primary_color, cx)
                                        } else {
                                            div().into_any_element()
                                        })
                                })
                                // 2) 字重与水平对齐
                                //
                                // 本行起的三组选项（字重对齐 / 文字描边 / 背景底框）都是
                                // 「固定宽标签 + 一排等分按钮」。按钮上必须同时写
                                // `flex_1()` 与 `min_w(px(0.0))`：只写 `flex_1` 时，
                                // flex 项的默认 `min-width:auto` 会按**内容最小宽度**顶住，
                                // 于是「细边 1.5」比「无描边」宽、整排参差不齐。
                                // 补上 `min_w(0)` 后三者才真正等分。
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .w(px(58.0))
                                                .flex_shrink_0()
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .child("字重对齐"),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                // 粗细开关
                                                .child(
                                                    div()
                                                        .w(px(96.0))
                                                        .flex()
                                                        .gap_1()
                                                        .child({
                                                            let is_bold = cur_style.is_bold;
                                                            div()
                                                                .id("btn-bold-on")
                                                                .flex_1()
                                                                .min_w(px(0.0))
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .font_weight(FontWeight::BOLD)
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(if is_bold { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_bold { Theme::accent_mint() } else { Theme::border_subtle() })
                                                                .text_color(if is_bold { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .child("加粗")
                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                    this.state.config.subtitle_style.is_bold = true;
                                                                    this.state.save_subtitle_style();
                                                                    cx.notify();
                                                                }))
                                                        })
                                                        .child({
                                                            let is_bold = cur_style.is_bold;
                                                            div()
                                                                .id("btn-bold-off")
                                                                .flex_1()
                                                                .min_w(px(0.0))
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(if !is_bold { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if !is_bold { Theme::accent_mint() } else { Theme::border_subtle() })
                                                                .text_color(if !is_bold { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .child("常规")
                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                    this.state.config.subtitle_style.is_bold = false;
                                                                    this.state.save_subtitle_style();
                                                                    cx.notify();
                                                                }))
                                                        })
                                                )
                                                // 对齐方式 (居左/居中/居右)
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .flex()
                                                        .gap_1()
                                                        .children([("靠左", "left"), ("居中", "center"), ("靠右", "right")].into_iter().enumerate().map(|(idx, (label, align))| {
                                                            let is_sel = cur_style.alignment == align;
                                                            div()
                                                                .id(("align-btn", idx))
                                                                .flex_1()
                                                                .min_w(px(0.0))
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { Theme::border_subtle() })
                                                                .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .child(label)
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.config.subtitle_style.alignment = align.to_string();
                                                                    this.state.save_subtitle_style();
                                                                    cx.notify();
                                                                }))
                                                        }))
                                                )
                                        )
                                )
                                // 3) 文字描边
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .w(px(58.0))
                                                .flex_shrink_0()
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .child("文字描边"),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .gap_1()
                                                .children(crate::utils::SUBTITLE_OUTLINE_OPTIONS.into_iter().enumerate().map(|(idx, (label, width))| {
                                                    let is_sel = (cur_style.outline_width - width).abs() < 0.1;
                                                    div()
                                                        .id(("outline-w", idx))
                                                        .flex_1()
                                                        .min_w(px(0.0))
                                                        .py_0p5()
                                                        .rounded_md()
                                                        .cursor_pointer()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                        .border_1()
                                                        .border_color(if is_sel { Theme::accent_mint() } else { Theme::border_subtle() })
                                                        .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                        .child(label)
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.state.config.subtitle_style.outline_width = width;
                                                            this.state.save_subtitle_style();
                                                            cx.notify();
                                                        }))
                                                }))
                                        )
                                )
                                // 4) 背景底框
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .w(px(58.0))
                                                .flex_shrink_0()
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .child("背景底框"),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .gap_1()
                                                .children([("投影", "shadow"), ("黑框", "box"), ("胶囊", "pill"), ("纯净", "none")].into_iter().enumerate().map(|(idx, (label, mode))| {
                                                    let is_sel = cur_style.bg_style == mode;
                                                    div()
                                                        .id(("bg-mode", idx))
                                                        .flex_1()
                                                        .min_w(px(0.0))
                                                        .py_0p5()
                                                        .rounded_md()
                                                        .cursor_pointer()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                        .border_1()
                                                        .border_color(if is_sel { Theme::accent_mint() } else { Theme::border_subtle() })
                                                        .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                        .child(label)
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.state.config.subtitle_style.bg_style = mode.to_string();
                                                            this.state.save_subtitle_style();
                                                            cx.notify();
                                                        }))
                                                }))
                                        )
                                )
                                // 3) 排版参数（字号 / 字间距 / 行间距 / 单行字数 / 底边距）
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1p5()
                                        // 字体大小
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("字体大小"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::FontSize,
                                                    "-2",
                                                    "+2",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.font_size;
                                                        this.state.config.subtitle_style.font_size = cur.saturating_sub(2).max(10);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.font_size;
                                                        this.state.config.subtitle_style.font_size = (cur + 2).min(120);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{}px", cur_style.font_size),
                                                    cx,
                                                ))
                                        )
                                        // 字间距
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("字间距"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::LetterSpacing,
                                                    "-1",
                                                    "+1",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.letter_spacing;
                                                        this.state.config.subtitle_style.letter_spacing = cur.saturating_sub(1);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.letter_spacing;
                                                        this.state.config.subtitle_style.letter_spacing = (cur + 1).min(50);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{}px", cur_style.letter_spacing),
                                                    cx,
                                                ))
                                        )
                                        // 底边距
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("底边距"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::BottomMargin,
                                                    "-5",
                                                    "+5",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.bottom_margin;
                                                        this.state.config.subtitle_style.bottom_margin = cur.saturating_sub(5);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.bottom_margin;
                                                        this.state.config.subtitle_style.bottom_margin = (cur + 5).min(500);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{}px", cur_style.bottom_margin),
                                                    cx,
                                                ))
                                        )
                                        // 行间距
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("行间距"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::LineSpacing,
                                                    "-0.1",
                                                    "+0.1",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.line_spacing;
                                                        let next = ((cur - 0.1) * 10.0).round() / 10.0;
                                                        this.state.config.subtitle_style.line_spacing = next.max(0.5);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.line_spacing;
                                                        let next = ((cur + 0.1) * 10.0).round() / 10.0;
                                                        this.state.config.subtitle_style.line_spacing = next.min(5.0);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{:.1}x", cur_style.line_spacing),
                                                    cx,
                                                ))
                                        )
                                        // 单行字数
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("单行字数"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::MaxChars,
                                                    "-1",
                                                    "+1",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.max_chars_per_line;
                                                        this.state.config.subtitle_style.max_chars_per_line = cur.saturating_sub(1).max(4);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.max_chars_per_line;
                                                        this.state.config.subtitle_style.max_chars_per_line = (cur + 1).min(100);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{}字", cur_style.max_chars_per_line),
                                                    cx,
                                                ))
                                        )
                                )
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    )
                    // 2. 单句快速编辑卡（仅「字幕样式」面板）。
                    //
                    // 这张卡自带「实时预览条」，预览的就是卡里正在编辑的这一句——编辑与
                    // 预览在同一张卡上，改字 / 调字号都是即时的。翻译面板不重复放编辑卡：
                    // 面板标头已能切回样式面板，两处各放一份只会让人分不清哪份生效。
                    .child(
                        if let Some(seg) = cur_seg.as_ref().filter(|_| panel == EditorSubtitlePanel::Style) {
                            let seg_idx = seg.index;
                            let start_ts = seconds_to_timestamp_short(seg.start);
                            let end_ts = seconds_to_timestamp_short(seg.end);
                            let dur = seg.duration();
                            // 单字无法拆分；最后一句没有下一句可合并，按钮置灰而不是点了没反应
                            let can_split = seg.display_text().chars().count() > 1;
                            let can_merge = seg_idx < self.state.segments.len();
                            // 正在导出片段时把两个导出按钮一并置灰：导出是后台任务，
                            // 重复点击会被忽略，显式不可点比「点了没反应」更清楚。
                            let clip_idle = !super::actions::CLIP_EXPORT_BUSY
                                .load(std::sync::atomic::Ordering::Relaxed);

                            primitives::card_sm()
                                .flex_none()
                                .gap(px(Theme::SPACE_2))
                                // 第一行：文段编号与主要操作 (保存修改、删除)
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(
                                                    div()
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_md()
                                                        .bg(Theme::tint_mint_badge())
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(Theme::accent_mint())
                                                        .child(format!("#{:03}", seg_idx)),
                                                )
                                                .child(
                                                    div()
                                                        .font_family("Consolas")
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child(format!("{} - {} ({:.1}s)", start_ts, end_ts, dur)),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .flex_shrink_0()
                                                .child(
                                                    div()
                                                        .id("btn-save-text-top")
                                                        .px_3()
                                                        .py_1()
                                                        .rounded_md()
                                                        .bg(Theme::accent_mint())
                                                        .cursor_pointer()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(Theme::text_on_accent())
                                                        .hover(|s| s.opacity(0.9))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }))
                                                        .child("保存修改"),
                                                )
                                                .child(
                                                    primitives::btn_danger("删除", primitives::BtnSize::Sm)
                                                        .id("btn-del-seg")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.delete_selected_segment();
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        })),
                                                ),
                                        ),
                                )
                                // 第一行半：次级操作工具条 (弹窗编辑、改译文、拆分、合并下句)
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .id("btn-open-prompt-edit")
                                                .px_2p5()
                                                .py_1()
                                                .rounded_md()
                                                .bg(Theme::bg_inset())
                                                .border_1()
                                                .border_color(Theme::border_mid())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.prompt_edit_text(cx);
                                                }))
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("弹窗编辑"),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .id("btn-open-translation-edit")
                                                .px_2p5()
                                                .py_1()
                                                .rounded_md()
                                                .bg(Theme::bg_inset())
                                                .border_1()
                                                .border_color(Theme::border_mid())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::bg_hover()))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    if let Some(idx) = this.state.selected_segment_index {
                                                        let trans = this.state.segments.iter().find(|s| s.index == idx)
                                                            .and_then(|s| s.translation.clone())
                                                            .unwrap_or_default();
                                                        this.start_inline_edit(idx, true, &trans, window, cx);
                                                    }
                                                }))
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("改译文"),
                                                ),
                                        )
                                        .child(
                                            if seg.has_translation() {
                                                div()
                                                    .id("btn-clear-translation")
                                                    .px_2p5()
                                                    .py_1()
                                                    .rounded_md()
                                                    .bg(Theme::bg_inset())
                                                    .border_1()
                                                    .border_color(Theme::border_mid())
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(Theme::bg_hover()))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.state.set_selected_translation("");
                                                        cx.notify();
                                                    }))
                                                    .child(
                                                        div()
                                                            .text_size(px(Theme::TEXT_SMALL))
                                                            .text_color(Theme::text_secondary())
                                                            .child("清除译文"),
                                                    )
                                                    .into_any_element()
                                            } else {
                                                div().into_any_element()
                                            },
                                        )
                                        .child(Self::render_mini_btn(
                                            "btn-split-seg",
                                            "拆分",
                                            can_split,
                                            cx,
                                            |this, cx| {
                                                let len = this.state.editing_text.chars().count();
                                                let at = if this.text_cursor_pos > 0
                                                    && this.text_cursor_pos < len
                                                {
                                                    Some(this.text_cursor_pos)
                                                } else {
                                                    None
                                                };
                                                this.state.split_selected_segment(at);
                                                this.trigger_extract_frame(cx);
                                            },
                                        ))
                                        .child(Self::render_mini_btn(
                                            "btn-merge-seg",
                                            "合并下句",
                                            can_merge,
                                            cx,
                                            |this, cx| {
                                                this.state.merge_selected_with_next();
                                                this.trigger_extract_frame(cx);
                                            },
                                        ))
                                        .child(Self::render_mini_btn(
                                            "btn-export-clip",
                                            "导出片段",
                                            clip_idle,
                                            cx,
                                            |this, cx| {
                                                this.export_selected_clip(cx);
                                            },
                                        ))
                                        .child(Self::render_mini_btn(
                                            "btn-export-clip-audio",
                                            "导出音频",
                                            clip_idle,
                                            cx,
                                            |this, cx| {
                                                this.export_selected_clip_audio(cx);
                                            },
                                        )),
                                )
                                // 第二行：原文输入框（带清晰标题）
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(Theme::text_muted())
                                                        .child("原文编辑"),
                                                ),
                                        )
                                        .child({
                                            let is_focused = self.is_text_focused;
                                            let total_chars = cur_text.chars().count();
                                            let cursor_pos = self.text_cursor_pos.min(total_chars);

                                            div()
                                                .id("subtitle-text-editor-box")
                                                .w_full()
                                                .track_focus(&self.text_focus)
                                                .min_h(px(Theme::TABLE_HEADER_H))
                                                .px(px(Theme::SPACE_3))
                                                .py(px(Theme::SPACE_1))
                                                .rounded(px(Theme::RADIUS_LG))
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(if is_focused { Theme::accent_mint() } else { Theme::border() })
                                                .cursor_text()
                                                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                                                    window.focus(&this.text_focus);
                                                    this.is_text_focused = true;
                                                    cx.notify();
                                                }))
                                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                                    let key = &event.keystroke.key;
                                                    let total_chars = this.state.editing_text.chars().count();
                                                    let cursor = this.text_cursor_pos.min(total_chars);

                                                    if event.keystroke.modifiers.control {
                                                        if key == "v" {
                                                            if let Some(item) = cx.read_from_clipboard() {
                                                                if let Some(text) = item.text() {
                                                                    let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                                                                    let insert_chars: Vec<char> = text.chars().collect();
                                                                    let ins_len = insert_chars.len();
                                                                    chars.splice(cursor..cursor, insert_chars);
                                                                    this.state.editing_text = chars.into_iter().collect();
                                                                    this.text_cursor_pos = cursor + ins_len;
                                                                    this.state.save_selected_text();
                                                                    cx.notify();
                                                                }
                                                            }
                                                        } else if key == "c" {
                                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(this.state.editing_text.clone()));
                                                            return;
                                                        } else if key == "a" {
                                                            this.text_cursor_pos = total_chars;
                                                            cx.notify();
                                                            return;
                                                        }
                                                        return;
                                                    }

                                                    if key == "backspace" {
                                                        if cursor > 0 && total_chars > 0 {
                                                            let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                                                            chars.remove(cursor - 1);
                                                            this.state.editing_text = chars.into_iter().collect();
                                                            this.text_cursor_pos = cursor - 1;
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }
                                                    } else if key == "delete" {
                                                        if cursor < total_chars {
                                                            let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                                                            chars.remove(cursor);
                                                            this.state.editing_text = chars.into_iter().collect();
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }
                                                    } else if key == "left" {
                                                        if cursor > 0 {
                                                            this.text_cursor_pos = cursor - 1;
                                                            cx.notify();
                                                        }
                                                    } else if key == "right" {
                                                        if cursor < total_chars {
                                                            this.text_cursor_pos = cursor + 1;
                                                            cx.notify();
                                                        }
                                                    } else if key == "home" {
                                                        this.text_cursor_pos = 0;
                                                        cx.notify();
                                                    } else if key == "end" {
                                                        this.text_cursor_pos = total_chars;
                                                        cx.notify();
                                                    } else if key == "enter" {
                                                        this.state.save_selected_text();
                                                        cx.notify();
                                                    } else if key.chars().count() == 1 {
                                                        let ch = key.chars().next().unwrap();
                                                        if !ch.is_control() {
                                                            let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                                                            chars.insert(cursor, ch);
                                                            this.state.editing_text = chars.into_iter().collect();
                                                            this.text_cursor_pos = cursor + 1;
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }
                                                    }
                                                }))
                                                .flex()
                                                .flex_wrap()
                                                .items_center()
                                                .text_size(px(Theme::TEXT_BODY_LG))
                                                .text_color(Theme::text_primary())
                                                .child(if cur_text.is_empty() {
                                                    div().child(if is_focused { "▌".to_string() } else { "点击直接输入字幕...".to_string() })
                                                } else {
                                                    div()
                                                        .flex()
                                                        .flex_wrap()
                                                        .items_center()
                                                        .children(cur_text.chars().enumerate().map(|(idx, ch)| {
                                                            let show_cursor = is_focused && cursor_pos == idx;
                                                            let ch_str = ch.to_string();
                                                            div()
                                                                .id(("text-char", idx))
                                                                .cursor_text()
                                                                .flex()
                                                                .items_center()
                                                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                    window.focus(&this.text_focus);
                                                                    this.is_text_focused = true;
                                                                    this.text_cursor_pos = idx;
                                                                    cx.notify();
                                                                }))
                                                                .child(if show_cursor {
                                                                    div()
                                                                        .flex()
                                                                        .items_center()
                                                                        .child(
                                                                            div()
                                                                                .text_color(Theme::accent_mint())
                                                                                .font_weight(FontWeight::BOLD)
                                                                                .child("▌"),
                                                                        )
                                                                        .child(ch_str)
                                                                    } else {
                                                                        div().child(ch_str)
                                                                    })
                                                        }))
                                                        .child(if is_focused && cursor_pos == total_chars {
                                                            div()
                                                                .cursor_text()
                                                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                    window.focus(&this.text_focus);
                                                                    this.is_text_focused = true;
                                                                    this.text_cursor_pos = total_chars;
                                                                    cx.notify();
                                                                }))
                                                                .text_color(Theme::accent_mint())
                                                                .font_weight(FontWeight::BOLD)
                                                                .child("▌")
                                                        } else {
                                                            div()
                                                        })
                                                })
                                        }),
                                )
                                // 第二行半：译文内容展示（若已翻译）
                                .children(seg.translation.as_ref().filter(|t| !t.trim().is_empty()).map(|trans_str| {
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(Theme::accent_mint())
                                                        .child("译文内容"),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .id("subtitle-trans-editor-box")
                                                .w_full()
                                                .min_h(px(Theme::TABLE_HEADER_H))
                                                .px(px(Theme::SPACE_3))
                                                .py(px(Theme::SPACE_1))
                                                .rounded(px(Theme::RADIUS_LG))
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .hover(|s| s.border_color(Theme::accent_mint()))
                                                .text_size(px(Theme::TEXT_BODY_LG))
                                                .text_color(Theme::accent_mint())
                                                .on_click(cx.listener({
                                                    let trans_clone = trans_str.clone();
                                                    let idx = seg.index;
                                                    move |this, _, window, cx| {
                                                        this.start_inline_edit(idx, true, &trans_clone, window, cx);
                                                    }
                                                }))
                                                .child(trans_str.clone()),
                                        )
                                }))
                                // 第二行四分之三：实时排版效果预览（带明确标题与双语联动）
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .pt_1()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(Theme::text_muted())
                                                        .child("画面效果预览"),
                                                ),
                                        )
                                        .child({
                                            let preview_str = if let Some(t) = &seg.translation {
                                                if !t.trim().is_empty() && t.trim() != cur_text.trim() {
                                                    format!("{}\n{}", t.trim(), cur_text.trim())
                                                } else {
                                                    cur_text.clone()
                                                }
                                            } else {
                                                cur_text.clone()
                                            };
                                            self.render_subtitle_preview_box(&preview_str, cx)
                                        }),
                                )
                                // 第三行：时间微调与快捷标点注入 (规整两行，干净不拥挤)
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(Theme::SPACE_2))
                                        // 第一行：时间微调（起止各 ±0.1s / ±0.5s）
                                        .child(
                                            div()
                                                .flex()
                                                .flex_wrap()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap_1()
                                                        .child(
                                                            div()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_color(Theme::text_muted())
                                                                .child("起点："),
                                                        )
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-start-minus-500",
                                                            "-0.5",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(-0.5, 0.0),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-start-minus-100",
                                                            "-0.1",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(-0.1, 0.0),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-start-plus-100",
                                                            "+0.1",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.1, 0.0),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-start-plus-500",
                                                            "+0.5",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.5, 0.0),
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap_1()
                                                        .child(
                                                            div()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_color(Theme::text_muted())
                                                                .child("终点："),
                                                        )
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-end-minus-500",
                                                            "-0.5",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.0, -0.5),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-end-minus-100",
                                                            "-0.1",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.0, -0.1),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-end-plus-100",
                                                            "+0.1",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.0, 0.1),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-end-plus-500",
                                                            "+0.5",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.0, 0.5),
                                                        )),
                                                ),
                                        )
                                        // 第二行：快捷标点注入
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("标点："),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap_1p5()
                                                        .child(self.render_punct_btn("，", cx))
                                                        .child(self.render_punct_btn("。", cx))
                                                        .child(self.render_punct_btn("？", cx))
                                                        .child(self.render_punct_btn("！", cx))
                                                        .child(self.render_punct_btn("、", cx)),
                                                ),
                                        ),
                                )
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    )
                    // 2.4 整轨时间轴调整（仅「字幕样式」面板可见）
                    //
                    // 与样式卡并列、而不是塞进样式卡内部：它是**整轨级**操作（平移 /
                    // 缩放 / 铺满），与「观感」是两件事；放在同一页是因为两者都属于
                    // 「我要调整这一整份字幕」的心智模型。
                    .child(if panel == EditorSubtitlePanel::Style {
                        self.render_retime_card(cx).into_any_element()
                    } else {
                        div().into_any_element()
                    })
                    // 2.5 字幕多语言翻译面板（仅「字幕翻译」面板可见；后端链路早已就绪）
                    .child(
                        if panel == EditorSubtitlePanel::Translate {
                            self.render_translate_card(cx).into_any_element()
                        } else if panel == EditorSubtitlePanel::Stats {
                            // 统计与导出同屏：两张卡都是「对整份字幕做事」的收尾动作，
                            // 一起竖排在同一个面板里。
                            div()
                                .w_full()
                                .flex_none()
                                .flex()
                                .flex_col()
                                .gap(px(Theme::CARD_GAP))
                                .child(self.render_subtitle_stats_card(cx))
                                .child(self.render_export_card(cx))
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        },
                    )
                    // 3. 多语言字幕配置与对照大表格 (仅在「字幕翻译」面板可见；「字幕样式」与「统计与导出」不展示此卡)
                    .child(if panel == EditorSubtitlePanel::Translate {
                        primitives::card_sm()
                            .flex_1()
                            .min_h(px(Theme::TABLE_MIN_H))
                            .gap(px(0.0))
                            // 上下堆叠时面板整体已可纵向滚动，这里不能再吃 flex_1
                            // （自动高度的滚动容器里会塌成 0 高），改为固定高度。
                            .when(stacked, |d| d.flex_none().h(px(Theme::TABLE_STACK_H)))
                            .overflow_hidden()
                            // 搜索栏：按关键字过滤清单（原文与译文都参与匹配），
                            // 命中条数实时回显，避免用户以为「搜了没结果」是卡住了
                            .child({
                                let total = self.state.segments.len();
                                let visible = self.subtitle_filter.len();
                                let searching = !self.subtitle_search.trim().is_empty();
                                div()
                                    .w_full()
                                    .h(px(Theme::TABLE_HEADER_H))
                                    .px(px(Theme::SPACE_3))
                                    .bg(Theme::bg_sidebar())
                                    .border_b_1()
                                    .border_color(Theme::border())
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .text_color(Theme::text_muted())
                                            .child("搜索"),
                                    )
                                    .child(self.render_search_input(cx))
                                    .child(if searching {
                                        div()
                                            .id("subtitle-search-clear")
                                            .flex_shrink_0()
                                            .px_2()
                                            .py_0p5()
                                            .rounded_md()
                                            .bg(Theme::bg_raised())
                                            .border_1()
                                            .border_color(Theme::border_mid())
                                            .cursor_pointer()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .text_color(Theme::text_secondary())
                                            .hover(|s| {
                                                s.bg(Theme::bg_hover())
                                                    .text_color(Theme::text_primary())
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.subtitle_search.clear();
                                                this.subtitle_search_cursor = 0;
                                                cx.notify();
                                            }))
                                            .child("清除")
                                            .into_any_element()
                                    } else {
                                        div().into_any_element()
                                    })
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .font_family("Consolas")
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .text_color(if searching && visible == 0 {
                                                Theme::accent_red()
                                            } else {
                                                Theme::text_muted()
                                            })
                                            .child(if searching {
                                                format!("命中 {}/{}", visible, total)
                                            } else {
                                                format!("共 {} 句", total)
                                            }),
                                    )
                                    .child(
                                        // 查找替换开关：与「搜索」分开成独立面板，因为
                                        // 搜索只过滤显示、替换会真改字幕，混在一起极易误操作。
                                        primitives::mini_btn("查找替换", true)
                                            .id("replace-panel-toggle")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.replace_panel_open = !this.replace_panel_open;
                                                if !this.replace_panel_open {
                                                    this.replace_status = None;
                                                }
                                                cx.notify();
                                            })),
                                    )
                            })
                            .children(self.render_replace_bar(cx))
                            // 列表信息子标头
                            .child({
                                // 双语判定与「有没有译文」都取自派生快照：以前这两条与下面
                                // 的行高计算、翻译卡里的同名判定各算一遍（同一件事每帧
                                // 三遍 O(n)），现在一次算完、按 revision 缓存。
                                let snapshot = self.derived_snapshot();
                                let has_any_trans = snapshot.has_any_translation;
                                let has_distinct_trans = snapshot.has_distinct_translation;
                                div()
                                    .w_full()
                                    .h(px(26.0))
                                    .px(px(Theme::SPACE_3))
                                    .bg(Theme::bg_sidebar())
                                    .border_b_1()
                                    .border_color(Theme::border())
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(Theme::text_muted())
                                            .child(if has_distinct_trans {
                                                "字幕清单（上下并列显示）"
                                            } else {
                                                "字幕清单"
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .text_color(if has_distinct_trans {
                                                Theme::accent_mint()
                                            } else {
                                                Theme::text_muted()
                                            })
                                            .child(if has_distinct_trans {
                                                "双语显示"
                                            } else if has_any_trans {
                                                "单语显示（译文同原文）"
                                            } else {
                                                "单语显示"
                                            }),
                                    )
                            })
                            // 表格行内容：uniform_list 虚拟化渲染，上下并列布局
                            .child({
                                // 选中行变化时自动滚动跟随（时间轴点击 / 上下句跳转 / 播放联动）
                                if self.subtitle_list_followed_sel != sel_idx {
                                    if let Some(idx) = sel_idx {
                                        // 滚动位置按「过滤后列表里的行号」计算，否则搜索态下会滚错行
                                        let pos = self.subtitle_filter.iter().position(|&p| {
                                            self.state.segments.get(p).map(|s| s.index) == Some(idx)
                                        });
                                        if let Some(pos) = pos {
                                            self.subtitle_list_scroll
                                                .scroll_to_item(pos, ScrollStrategy::Top);
                                        }
                                    }
                                    self.subtitle_list_followed_sel = sel_idx;
                                }

                                let row_count = self.subtitle_filter.len();
                                // 行高与上面子标头必须用同一个布尔，否则会出现
                                // 「标题写双语、行高是单语」。都取快照。
                                let has_distinct_trans =
                                    self.derived_snapshot().has_distinct_translation;
                                let row_h = if has_distinct_trans {
                                    SUBTITLE_ROW_H_BILINGUAL
                                } else {
                                    SUBTITLE_ROW_H_MONO
                                };

                                // 术语表合规：疑似未按术语表译出的句子下标集合。每帧只算一次
                                let glossary_bad: std::collections::HashSet<usize> = self
                                    .cached_glossary_violations()
                                    .into_iter()
                                    .collect();
                                // 低置信句集合：同样每帧只算一次
                                let low_conf: std::collections::HashSet<usize> = self
                                    .cached_low_confidence()
                                    .into_iter()
                                    .collect();
                                uniform_list(
                                    "inspector-segments-virtual",
                                    row_count,
                                    cx.processor(move |this, visible_range: std::ops::Range<usize>, _window, cx| {
                                        let sel = this.state.selected_segment_index;
                                        let cur_time = this.state.current_time;
                                        let glossary_bad = glossary_bad.clone();
                                        let low_conf = low_conf.clone();
                                        let inline_target = this.inline_edit_target;
                                        let inline_buf = this.inline_edit_buffer.clone();
                                        let inline_focus = this.inline_edit_focus.clone();
                                        visible_range
                                            .map(|i| {
                                                let pos = this.subtitle_filter.get(i).copied().unwrap_or(i);
                                                let Some(seg) = this.state.segments.get(pos) else {
                                                    return div()
                                                        .id(("table-row-placeholder", i))
                                                        .w_full()
                                                        .h(px(row_h))
                                                        .border_b_1()
                                                        .border_color(Theme::border_subtle())
                                                        .into_any_element();
                                                };
                                                let seg_idx = seg.index;
                                                let is_selected = sel == Some(seg_idx);
                                                let is_playing_here = is_playing_at(seg.start, seg.end, cur_time);
                                                let start_ts = seconds_to_timestamp_short(seg.start);
                                                let end_ts = seconds_to_timestamp_short(seg.end);
                                                let dur = seg.duration();
                                                let raw_text = seg.display_text().to_string();
                                                let has_trans = seg.has_translation();
                                                let trans_text = seg.translation.clone().unwrap_or_default();
                                                let speaker = seg.speaker;
                                                let glossary_flagged = glossary_bad.contains(&seg_idx);
                                                let low_conf_flagged = low_conf.contains(&seg_idx);

                                                div()
                                                    .id(("table-row-seg", seg_idx))
                                                    .w_full()
                                                    .h(px(row_h))
                                                    .px(px(Theme::SPACE_3))
                                                    .py(px(4.0))
                                                    .border_b_1()
                                                    .border_color(Theme::border_subtle())
                                                    .cursor_pointer()
                                                    .relative()
                                                    .bg(if is_selected {
                                                        Theme::tint_mint_soft()
                                                    } else if glossary_flagged {
                                                        Theme::tint_warn_soft()
                                                    } else if is_playing_here {
                                                        Theme::tint_primary_soft()
                                                    } else {
                                                        Theme::transparent()
                                                    })
                                                    .hover(|s| s.bg(if is_selected { Theme::tint_mint_badge() } else { Theme::bg_hover() }))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        // 点击某行 = 跳播到该句起点。先停掉正在进行的实时播放：
                                                        // 否则播放时钟会在下一帧把 current_time 拉回旧位置、音频也
                                                        // 继续从旧处播放，点击等于没生效（与键盘「上/下句」
                                                        // `jump_prev/next_segment` 同一套「先停再选」语义）。
                                                        this.halt_preview_playback();
                                                        this.state.select_segment(seg_idx);
                                                        // 选中即把播放位置归位到该句起点；再显式 seek 一次做边界钳制，
                                                        // 随后 `trigger_extract_frame` 抽帧，画面即落到这一句。
                                                        if let Some(target) =
                                                            click_seek_target(&this.state.segments, seg_idx)
                                                        {
                                                            this.state.seek_to(target);
                                                        }
                                                        this.trigger_extract_frame(cx);
                                                        cx.notify();
                                                    }))
                                                    .flex()
                                                    .flex_col()
                                                    .justify_start()
                                                    .gap(px(3.0))
                                                    // 选中指示线（左缘 3px 竖条）
                                                    .child(if is_selected {
                                                        div()
                                                            .absolute()
                                                            .left(px(0.0))
                                                            .top(px(2.0))
                                                            .bottom(px(2.0))
                                                            .w(px(3.0))
                                                            .rounded_r_full()
                                                            .bg(Theme::accent_mint())
                                                            .into_any_element()
                                                    } else {
                                                        div().into_any_element()
                                                    })
                                                    // 第一行：时间与元数据（序号、起止时间、时长、说话人、标记）
                                                    .child(
                                                        div()
                                                            .w_full()
                                                            .flex()
                                                            .items_center()
                                                            .justify_between()
                                                            .child(
                                                                div()
                                                                    .flex()
                                                                    .items_center()
                                                                    .gap_1p5()
                                                                    .children(low_conf_flagged.then(|| {
                                                                        primitives::stat_dot_sm(Theme::accent_red())
                                                                    }))
                                                                    .child(
                                                                        div()
                                                                            .px_1()
                                                                            .py_0p5()
                                                                            .rounded_sm()
                                                                            .bg(if is_selected {
                                                                                Theme::tint_mint_badge()
                                                                            } else {
                                                                                Theme::bg_raised()
                                                                            })
                                                                            .text_size(px(Theme::TEXT_CAPTION))
                                                                            .font_weight(FontWeight::BOLD)
                                                                            .text_color(if is_selected {
                                                                                Theme::accent_mint()
                                                                            } else {
                                                                                Theme::text_muted()
                                                                            })
                                                                            .child(format!("#{:03}", seg_idx)),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .font_family("Consolas")
                                                                            .text_size(px(Theme::TEXT_SMALL))
                                                                            .text_color(if is_selected {
                                                                                Theme::accent_mint()
                                                                            } else {
                                                                                Theme::text_secondary()
                                                                            })
                                                                            .child(format!("{} - {}", start_ts, end_ts)),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_size(px(Theme::TEXT_CAPTION))
                                                                            .text_color(Theme::text_muted())
                                                                            .child(format!("({:.1}s)", dur)),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .flex()
                                                                    .items_center()
                                                                    .gap_1()
                                                                    .children(speaker.map(|s| {
                                                                        div()
                                                                            .px_1p5()
                                                                            .py_0p5()
                                                                            .rounded_sm()
                                                                            .border_1()
                                                                            .border_color(speaker_border(s))
                                                                            .bg(speaker_tint(s))
                                                                            .text_size(px(Theme::TEXT_CAPTION))
                                                                            .font_weight(FontWeight::SEMIBOLD)
                                                                            .text_color(speaker_color(s))
                                                                            .child(format!("说话人 {}", s + 1))
                                                                    }))
                                                                    .children(glossary_flagged.then(|| {
                                                                        div()
                                                                            .px_1()
                                                                            .py_0p5()
                                                                            .rounded_sm()
                                                                            .bg(Theme::tint_warn_soft())
                                                                            .text_size(px(Theme::TEXT_CAPTION))
                                                                            .text_color(Theme::accent_orange())
                                                                            .child("术语")
                                                                    })),
                                                            ),
                                                    )
                                                    // 第二行：字幕内容 (原文，点击直接就地编辑)
                                                    .child({
                                                        let is_editing_raw = inline_target == Some((seg_idx, false));
                                                        if is_editing_raw {
                                                            div()
                                                                .id(("inline-edit-raw", seg_idx))
                                                                .w_full()
                                                                .h(px(24.0))
                                                                .track_focus(&inline_focus)
                                                                .px_1p5()
                                                                .py_0p5()
                                                                .rounded_sm()
                                                                .bg(Theme::bg_sidebar())
                                                                .border_1()
                                                                .border_color(Theme::accent_mint())
                                                                .cursor_text()
                                                                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                                                                    window.focus(&this.inline_edit_focus);
                                                                    cx.notify();
                                                                }))
                                                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                                                    this.handle_inline_edit_keydown(event, cx);
                                                                }))
                                                                .flex()
                                                                .items_center()
                                                                .justify_between()
                                                                .child(
                                                                    div()
                                                                        .flex_1()
                                                                        .text_size(px(Theme::TEXT_BODY))
                                                                        .text_color(Theme::accent_mint())
                                                                        .truncate()
                                                                        .child(format!("{}▌", inline_buf)),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .flex()
                                                                        .items_center()
                                                                        .gap_1()
                                                                        .flex_shrink_0()
                                                                        .child(
                                                                            div()
                                                                                .id(("inline-save-raw", seg_idx))
                                                                                .px_1()
                                                                                .py_0p5()
                                                                                .rounded_sm()
                                                                                .bg(Theme::accent_mint())
                                                                                .text_size(px(10.0))
                                                                                .text_color(Theme::text_on_accent())
                                                                                .cursor_pointer()
                                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                                    this.commit_inline_edit(cx);
                                                                                }))
                                                                                .child("保存"),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .id(("inline-cancel-raw", seg_idx))
                                                                                .px_1()
                                                                                .py_0p5()
                                                                                .rounded_sm()
                                                                                .bg(Theme::bg_hover())
                                                                                .text_size(px(10.0))
                                                                                .text_color(Theme::text_muted())
                                                                                .cursor_pointer()
                                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                                    this.cancel_inline_edit(cx);
                                                                                }))
                                                                                .child("取消"),
                                                                        ),
                                                                )
                                                        } else {
                                                            div()
                                                                .id(("table-row-raw", seg_idx))
                                                                .w_full()
                                                                .text_size(px(Theme::TEXT_BODY))
                                                                .font_weight(if is_selected {
                                                                    FontWeight::SEMIBOLD
                                                                } else {
                                                                    FontWeight::NORMAL
                                                                })
                                                                .text_color(Theme::text_primary())
                                                                .truncate()
                                                                .hover(|s| s.text_color(Theme::accent_mint()))
                                                                .cursor_pointer()
                                                                .on_click(cx.listener({
                                                                    let raw_for_click = raw_text.clone();
                                                                    move |this, _, window, cx| {
                                                                        this.state.select_segment(seg_idx);
                                                                        this.trigger_extract_frame(cx);
                                                                        let now = std::time::Instant::now();
                                                                        let is_double = matches!(
                                                                            this.last_subtitle_click,
                                                                            Some((last_idx, false, last_t))
                                                                                if last_idx == seg_idx && now.duration_since(last_t).as_millis() < 350
                                                                        );
                                                                        if is_double {
                                                                            this.last_subtitle_click = None;
                                                                            this.start_inline_edit(seg_idx, false, &raw_for_click, window, cx);
                                                                        } else {
                                                                            this.last_subtitle_click = Some((seg_idx, false, now));
                                                                            cx.notify();
                                                                        }
                                                                    }
                                                                }))
                                                                .child(raw_text.clone())
                                                        }
                                                    })
                                                    // 第三行：翻译字幕 (点击直接就地编辑；无翻译且非编辑态时不显示)
                                                    .children({
                                                        let is_editing_trans = inline_target == Some((seg_idx, true));
                                                        let show_trans = is_editing_trans || (has_trans && !trans_text.trim().is_empty() && trans_text.trim() != raw_text.trim());
                                                        if show_trans {
                                                            Some(if is_editing_trans {
                                                                div()
                                                                    .id(("inline-edit-trans", seg_idx))
                                                                    .w_full()
                                                                    .h(px(24.0))
                                                                    .track_focus(&inline_focus)
                                                                    .px_1p5()
                                                                    .py_0p5()
                                                                    .rounded_sm()
                                                                    .bg(Theme::bg_sidebar())
                                                                    .border_1()
                                                                    .border_color(Theme::accent_mint())
                                                                    .cursor_text()
                                                                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                                                                        window.focus(&this.inline_edit_focus);
                                                                        cx.notify();
                                                                    }))
                                                                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                                                        this.handle_inline_edit_keydown(event, cx);
                                                                    }))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_between()
                                                                    .child(
                                                                        div()
                                                                            .flex_1()
                                                                            .text_size(px(Theme::TEXT_SMALL))
                                                                            .text_color(Theme::accent_mint())
                                                                            .truncate()
                                                                            .child(format!("{}▌", inline_buf)),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .flex()
                                                                            .items_center()
                                                                            .gap_1()
                                                                            .flex_shrink_0()
                                                                            .child(
                                                                                div()
                                                                                    .id(("inline-save-trans", seg_idx))
                                                                                    .px_1()
                                                                                    .py_0p5()
                                                                                    .rounded_sm()
                                                                                    .bg(Theme::accent_mint())
                                                                                    .text_size(px(10.0))
                                                                                    .text_color(Theme::text_on_accent())
                                                                                    .cursor_pointer()
                                                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                                                        this.commit_inline_edit(cx);
                                                                                    }))
                                                                                    .child("保存"),
                                                                            )
                                                                            .child(
                                                                                div()
                                                                                    .id(("inline-cancel-trans", seg_idx))
                                                                                    .px_1()
                                                                                    .py_0p5()
                                                                                    .rounded_sm()
                                                                                    .bg(Theme::bg_hover())
                                                                                    .text_size(px(10.0))
                                                                                    .text_color(Theme::text_muted())
                                                                                    .cursor_pointer()
                                                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                                                        this.cancel_inline_edit(cx);
                                                                                    }))
                                                                                    .child("取消"),
                                                                            ),
                                                                    )
                                                            } else {
                                                                div()
                                                                    .id(("table-row-trans", seg_idx))
                                                                    .w_full()
                                                                    .text_size(px(Theme::TEXT_SMALL))
                                                                    .text_color(if is_selected {
                                                                        Theme::accent_mint()
                                                                    } else {
                                                                        Theme::text_secondary()
                                                                    })
                                                                    .truncate()
                                                                    .hover(|s| s.text_color(Theme::accent_mint()))
                                                                    .cursor_pointer()
                                                                    .on_click(cx.listener({
                                                                        let trans_for_click = trans_text.clone();
                                                                        move |this, _, window, cx| {
                                                                            this.state.select_segment(seg_idx);
                                                                            this.trigger_extract_frame(cx);
                                                                            let now = std::time::Instant::now();
                                                                            let is_double = matches!(
                                                                                this.last_subtitle_click,
                                                                                Some((last_idx, true, last_t))
                                                                                    if last_idx == seg_idx && now.duration_since(last_t).as_millis() < 350
                                                                            );
                                                                            if is_double {
                                                                                this.last_subtitle_click = None;
                                                                                this.start_inline_edit(seg_idx, true, &trans_for_click, window, cx);
                                                                            } else {
                                                                                this.last_subtitle_click = Some((seg_idx, true, now));
                                                                                cx.notify();
                                                                            }
                                                                        }
                                                                    }))
                                                                    .child(trans_text)
                                                            })
                                                        } else {
                                                            None
                                                        }
                                                    })
                                                    .into_any_element()
                                            })
                                            .collect()
                                    }),
                                )
                                .track_scroll(self.subtitle_list_scroll.clone())
                                .flex_1()
                                .w_full()
                                // 内嵌清单自己吃掉滚轮：清单还有余量时不让事件继续冒泡到
                                // 外层可滚动面板，避免「清单与外层面板一起滚」（见
                                // `should_consume_scroll` 的说明）。到边界时放行，外层接管。
                                .on_scroll_wheel(cx.listener(
                                    |this, event: &ScrollWheelEvent, _window, cx| {
                                        let delta_y = match event.delta {
                                            ScrollDelta::Lines(p) => p.y,
                                            ScrollDelta::Pixels(p) => f32::from(p.y),
                                        };
                                        let state = this.subtitle_list_scroll.0.borrow();
                                        let offset_y =
                                            f32::from(state.base_handle.offset().y);
                                        let max_h = f32::from(state.base_handle.max_offset().height);
                                        drop(state);
                                        if primitives::should_consume_scroll(delta_y, offset_y, max_h) {
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                            })
                            .into_any_element()
                    } else {
                        div().into_any_element()
                    })
            )
    }

    /// 字幕预览框：显示当前编辑句的排版效果，两侧把手可拖拽调宽。
    ///
    /// 拖动任一手的位移按**两倍**作用到宽度上——把手贴在字幕框的两条边上，
    /// 往右拖 d 像素，右边多 d、左边也多 d，于是框以中线为中心左右对称变宽 / 变窄。
    /// 手动拖过之后宽度以手动值为准（`preview_box_w`），否则按「单行最大字数 ×
    /// 预览字号」自动推算。
    fn render_subtitle_preview_box(
        &mut self,
        cur_text: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 样式取自全局配置（面板里改哪个参数都立即写回这里）
        let style = self.state.config.subtitle_style.clone();
        let font_px = style.preview_font_px();
        let (fg, bg, border) = subtitle_computed_colors(&style);
        let bg = preview_backdrop(bg);
        // 空行会被 GPUI 折成 0 高，给个占位符保证预览条始终有形
        let preview_text = if cur_text.trim().is_empty() {
            "（本句暂无文字）".to_string()
        } else {
            cur_text.to_string()
        };
        let box_w = self.subtitle_box_w();
        let is_dragging = self.preview_drag.is_some();

        div()
            .id("subtitle-preview-row")
            .w_full()
            .h(px(Theme::STYLE_PREVIEW_H))
            .rounded(px(Theme::RADIUS_LG))
            .bg(Theme::bg_media())
            .border_1()
            .border_color(if is_dragging {
                Theme::accent_mint()
            } else {
                Theme::border()
            })
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            // 位移监听挂在整个预览行上而不是把手上：把手只有几像素宽，拖动时指针
            // 稍一移动就出了它的命中区；整行满宽，横向拖多远都不会掉出去。
            //
            // 必须再校验 `event.pressed_button`：GPUI 的 `on_mouse_move` 只看指针**当前**
            // 是否落在元素内，并不关心按钮是不是在这个元素上按下的。少了这层校验，指针
            // 在别处按下、再移进本行时会被误当成「正在拖预览框」——`drag_preview_box`
            // 见 `preview_drag` 是 `Some` 就一直改宽度。更糟的是若按钮是在把手之外
            // 松开的，本行的 `on_mouse_up` 收不到（松手时指针不在行内），`preview_drag`
            // 永远不清空，此后指针每次划过预览行都会继续改宽度。
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                this.drag_preview_box(event.position.x, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_preview_box_drag();
                    cx.notify();
                }),
            )
            // 在行**外**松手也要收尾。GPUI 的 `on_mouse_up` 同样要求指针落在元素内，
            // 若用户把指针拖出行外才松手，上面的 handler 收不到，`preview_drag` 会一直
            // 挂着——预览行从此恒亮「拖拽中」描边，且宽度卡在半途的中间值。
            // `on_mouse_up_out` 走捕获阶段、专在「松手时指针不在元素内」时触发，用它兜底。
            // 没有拖拽会话时 `end_preview_box_drag` 是空操作，因此它在任何别处松手都安全。
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_preview_box_drag();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(self.preview_resize_handle(true, cx))
                    .child(
                        div()
                            .w(px(box_w))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(bg)
                            .border_1()
                            .border_color(border)
                            .text_size(px(font_px))
                            .font_weight(if style.is_bold {
                                FontWeight::BOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(fg)
                            .text_align(match style.alignment.as_str() {
                                "left" => TextAlign::Left,
                                "right" => TextAlign::Right,
                                _ => TextAlign::Center,
                            })
                            .line_height(px(font_px * style.line_spacing))
                            .child(preview_text),
                    )
                    .child(self.preview_resize_handle(false, cx)),
            )
    }

    /// 预览框的拖拽把手，贴在字幕框左右两侧；`left` 决定是左只还是右只。
    fn preview_resize_handle(&self, left: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.preview_drag.is_some();
        div()
            .id(if left {
                "preview-handle-left"
            } else {
                "preview-handle-right"
            })
            .w(px(Theme::RESIZE_HANDLE_W))
            .h(px(Theme::STYLE_PREVIEW_H - Theme::SPACE_3 * 2.0))
            .rounded(px(Theme::RADIUS_SM))
            .bg(if active {
                Theme::accent_mint()
            } else {
                Theme::border_strong()
            })
            .opacity(if active { 1.0 } else { 0.5 })
            .cursor_col_resize()
            .hover(|s| s.opacity(1.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.begin_preview_box_drag(event.position.x, left);
                    cx.notify();
                }),
            )
    }

    /// 字幕框的统一宽度（px）。
    ///
    /// 监视器覆盖层与编辑卡预览条**共用这一个来源**，所以拖把手时两者在同一帧一起
    /// 变化，不存在「预览改了、画面没跟上」的延迟。手动拖过就用 `preview_box_w`，
    /// 否则按「单行最大字数 × 预览字号」自动推算。
    fn subtitle_box_w(&self) -> f32 {
        let s = &self.state.config.subtitle_style;
        self.preview_box_w
            .unwrap_or_else(|| s.preview_font_px() * s.max_chars_per_line as f32)
            .clamp(Theme::PREVIEW_BOX_MIN_W, Theme::PREVIEW_BOX_MAX_W)
    }

    /// 开始拖拽预览框：记下基准宽度、按下时的鼠标 x、以及抓的是哪一侧把手。
    fn begin_preview_box_drag(&mut self, mouse_x: Pixels, left: bool) {
        self.preview_drag = Some((self.subtitle_box_w(), f32::from(mouse_x), left));
    }

    /// 拖拽中：位移按两倍作用到宽度（两侧对称），结果记进手动宽度。
    ///
    /// 左右把手的「往外」方向相反，所以按手柄所在侧取符号：右手柄往右拉（dx>0）
    /// 加宽，左手柄往左拉（dx<0）同样加宽。少了这一步符号，左手柄往外拉会算成
    /// 负增量，看着就是「往外拉反而变窄」。
    fn drag_preview_box(&mut self, mouse_x: Pixels, cx: &mut Context<Self>) {
        let Some((start_w, start_x, left)) = self.preview_drag else {
            return;
        };
        let sign = if left { -1.0 } else { 1.0 };
        let new_w = (start_w + sign * 2.0 * (f32::from(mouse_x) - start_x))
            .clamp(Theme::PREVIEW_BOX_MIN_W, Theme::PREVIEW_BOX_MAX_W);
        self.preview_box_w = Some(new_w);
        cx.notify();
    }

    /// 松手：结束拖拽会话，并把宽度落盘（下次启动仍用这个宽度）。
    fn end_preview_box_drag(&mut self) {
        if self.preview_drag.take().is_some() {
            self.state.config.subtitle_style.preview_box_w = self.preview_box_w;
            self.state.save_subtitle_style();
        }
    }

    /// 整轨时间轴调整卡：平移 / 缩放 / 铺满目标时长，以及句间最小间隔档位。
    ///
    /// # 为什么需要它
    ///
    /// 字幕的时间轴几乎从不对上最终成片：片子剪掉了 12 秒就要整轨平移；开头加了一段
    /// 片头就要「某点之后平移」；25 分钟的成片复用 22 分钟的版本就要整体缩放；TXT
    /// 导入合成的时间轴更是完全假的、必须铺满真实时长。此前这些只能一行行拖。
    ///
    /// # 设计要点
    ///
    /// - **先算后应用**：规划在 `subtitle::timing`（纯函数、16 条单测），这里只负责
    ///   取参数、显示预览摘要、点「应用」才写回。用户可以反复试参数而不用担心
    ///   把时间轴改坏——没点应用就没有任何副作用。
    /// - **应用前记撤销快照**：整轨操作一改就是几百句，必须有路可退。
    /// - **重叠自纠**：变换后若产生重叠（缩放比例与原始空隙不一致时常见），应用时
    ///   自动过一遍 `resolve_overlaps`，保证结果仍能原样通过 `optimize_segments`
    ///   ——否则用户下次载入工程会被「排序 + 截断重叠」再改一遍，看起来像怎么改都丢。
    fn render_retime_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::subtitle::timing::{
            describe, plan_min_gaps, plan_retime, suggest_fit, TimeShift,
        };
        let segments = &self.state.segments;
        // 三个参数：平移量、缩放比、铺满目标。都用整数档位（渲染器只吃 u32），
        // 显示时换算回秒 / 百分比。
        let shift_ms = self.retime_shift_ms;
        let scale_pct = self.retime_scale_pct;
        let target_secs = self.retime_target_secs;
        let op = self.retime_op;
        // 「句间最小间隔」：与平移 / 缩放 / 铺满共用同一个「当前方式」选择位
        // （`retime_op` 的 4..=6 对应 [`MIN_GAP_PRESETS`] 三档，0..=3 语义不动）。
        let gap_span = MIN_GAP_OP_BASE + MIN_GAP_PRESETS.len() as u8;
        let gap_idx: Option<usize> = if (MIN_GAP_OP_BASE..gap_span).contains(&op) {
            Some((op - MIN_GAP_OP_BASE) as usize)
        } else {
            None
        };
        // 预览：按当前档位在**副本**上算一遍，不改任何字幕（与整轨变换同一形状）。
        let gap_plan = gap_idx.map(|i| plan_min_gaps(segments, MIN_GAP_PRESETS[i]));

        // 预览：按当前参数算一遍，把「会改几句 / 是否重叠」如实显示出来。
        let preview = match op {
            1 => Some(plan_retime(
                segments,
                TimeShift::ShiftAll {
                    delta: shift_ms as f64 / 1000.0,
                },
            )),
            2 => Some(plan_retime(
                segments,
                TimeShift::Scale {
                    source_span: 1.0,
                    target_span: scale_pct as f64 / 100.0,
                },
            )),
            3 => Some(plan_retime(
                segments,
                TimeShift::FitToDuration {
                    target_secs: target_secs as f64,
                },
            )),
            _ => None,
        };

        let preview_text = match (&preview, op) {
            (Some(plan), _) => {
                let shift_desc = match op {
                    1 => describe(TimeShift::ShiftAll {
                        delta: shift_ms as f64 / 1000.0,
                    }),
                    2 => format!("缩放 {:+.0}%", scale_pct as f64 - 100.0),
                    _ => format!("铺满 {target_secs} 秒"),
                };
                let mut t = format!("{shift_desc} · 将改 {} 句", plan.changed);
                if plan.overlaps {
                    // 重叠不是错误——应用时会自动压掉——但必须提前说，否则用户看到
                    // 「应用后句数没变但时间变了」会以为是 bug。
                    t.push_str(" · 检测到重叠，应用时自动压平");
                }
                if plan.degenerate {
                    t.push_str(" · 含过短片段，应用时会自纠到最短时长");
                }
                t
            }
            (None, _) if gap_plan.is_some() => {
                // 当前生效的是句间间隔档位：整轨变换这一栏没有参数可显示，不要停在
                // 「选择一种调整方式」上，让用户以为漏选了什么。
                "整轨变换未启用".to_string()
            }
            (None, _) => "选择一种调整方式".to_string(),
        };

        // 目标时长的默认建议：整轨已有时长与视频总时长差得远时，界面直接提示
        // （TXT 导入后必然命中）。
        let fit_hint = suggest_fit(segments, self.state.total_duration);

        let op_row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(
                primitives::segmented("整轨平移", op == 1, false)
                    .id("retime-op-shift")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.retime_op = 1;
                        cx.notify();
                    })),
            )
            .child(
                primitives::segmented("整体缩放", op == 2, false)
                    .id("retime-op-scale")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.retime_op = 2;
                        cx.notify();
                    })),
            )
            .child(
                primitives::segmented("铺满总时长", op == 3, false)
                    .id("retime-op-fit")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.retime_op = 3;
                        cx.notify();
                    })),
            );

        // 参数行：按当前方式只显示相关的那一项，避免三个滑条堆在一起让人不知道
        // 哪个在生效。
        let param_row = match op {
            1 => self
                .render_slider(
                    "retime-slider-shift",
                    "平移量",
                    // 档位 = (毫秒/1000 + 5) 取整，范围 0..60；负值也落在档位内。
                    ((shift_ms / 1000) + 5).clamp(0, 60) as u32,
                    0,
                    60,
                    format!("{:+.1} s", shift_ms as f64 / 1000.0),
                    "正数往后、负数往前；起点会被钳在 0 且保持时长不变".to_string(),
                    cx,
                    move |this, v, cx| {
                        // 滑条档位 0..60 秒 → -5.0..+55.0 秒，覆盖常见的修剪幅度。
                        this.retime_shift_ms = (v as i64 - 5) * 1000;
                        cx.notify();
                    },
                )
                .into_any_element(),
            2 => self
                .render_slider(
                    "retime-slider-scale",
                    "缩放比例",
                    scale_pct,
                    50,
                    150,
                    format!("{:.0}%", scale_pct),
                    "成片比字幕版本更长就把比例调大（时间轴等比拉伸）".to_string(),
                    cx,
                    move |this, v, cx| {
                        this.retime_scale_pct = v;
                        cx.notify();
                    },
                )
                .into_any_element(),
            3 => self
                .render_slider(
                    "retime-slider-target",
                    "目标总时长",
                    target_secs,
                    10,
                    3600,
                    crate::utils::time::format_duration_short(target_secs as f64),
                    fit_hint
                        .map(|_| {
                            format!(
                                "当前整轨 {}，与视频时长不符——建议铺满到 {}",
                                crate::utils::time::format_duration_short(
                                    self.state.segments.last().map(|s| s.end).unwrap_or(0.0)
                                ),
                                crate::utils::time::format_duration_short(
                                    self.state.total_duration
                                )
                            )
                        })
                        .unwrap_or_else(|| "按各句时长比例重新铺满整轨".to_string()),
                    cx,
                    move |this, v, cx| {
                        this.retime_target_secs = v;
                        cx.notify();
                    },
                )
                .into_any_element(),
            _ => div().into_any_element(),
        };

        // 只有平移 / 缩放 / 铺满走这个按钮（`retime_op` 1..=3）；4..=6 是句间间隔
        // 档位，它们有自己的「应用间隔」——这里必须置灰，否则会亮着一个点了没反应的
        // 按钮。
        let can_apply = matches!(op, 1..=3) && !segments.is_empty();
        let apply_label = match op {
            1 => "应用平移",
            2 => "应用缩放",
            3 => "铺满时间轴",
            _ => "应用",
        };
        let apply_row = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex_1()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(preview_text),
            )
            .child(
                primitives::mini_btn(apply_label, can_apply)
                    .id("retime-apply")
                    .when(can_apply, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            this.apply_retime(cx);
                        }))
                    }),
            );

        // 「句间最小间隔」一组（追加在整轨变换之后，不改动上面三行的排布）：档位胶囊
        // + 一行小字 + 自己的「应用间隔」。形状与整轨变换一致——先在副本上算
        // （`gap_plan`）→ 小字里显示会改几句 → 点应用才写回。
        let gap_ready = gap_idx.is_some() && !segments.is_empty();
        let gap_row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .children(MIN_GAP_PRESETS.iter().enumerate().map(|(i, secs)| {
                primitives::chip_clickable(format!("{secs:.2}s"), gap_idx == Some(i), false)
                    .id(("min-gap-preset", i))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.retime_op = MIN_GAP_OP_BASE + i as u8;
                        cx.notify();
                    }))
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(match &gap_plan {
                        Some(plan) => format!("{MIN_GAP_INTRO} · {}", min_gap_hint(plan, false)),
                        None => MIN_GAP_INTRO.to_string(),
                    }),
            )
            .child(
                primitives::mini_btn("应用间隔", gap_ready)
                    .id("min-gap-apply")
                    .when(gap_ready, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.apply_min_gap(cx);
                        }))
                    }),
            );

        primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(primitives::section_title("整轨时间轴调整")),
            )
            .child(op_row)
            .child(param_row)
            .child(apply_row)
            .child(gap_row)
    }

    /// 把当前参数对应的整轨变换真正写回字幕，并自纠重叠。
    ///
    /// 与 `render_retime_card` 的预览用**同一份** `plan_retime` 调用参数，避免
    /// 「预览说改 190 句、应用却改了 189 句」这种自相矛盾。
    fn apply_retime(&mut self, cx: &mut Context<Self>) {
        use crate::subtitle::timing::{plan_retime, resolve_overlaps, TimeShift};
        if self.state.segments.is_empty() {
            return;
        }
        let shift = match self.retime_op {
            1 => TimeShift::ShiftAll {
                delta: self.retime_shift_ms as f64 / 1000.0,
            },
            2 => TimeShift::Scale {
                source_span: 1.0,
                target_span: self.retime_scale_pct as f64 / 100.0,
            },
            3 => TimeShift::FitToDuration {
                target_secs: self.retime_target_secs as f64,
            },
            _ => return,
        };
        let plan = plan_retime(&self.state.segments, shift);
        // 应用前记快照：整轨操作一改几百句，点错了必须有路可退。
        self.state.snapshot_for_undo();
        let resolved = resolve_overlaps(&plan.times);
        for (seg, (start, end)) in self.state.segments.iter_mut().zip(resolved) {
            seg.start = start;
            seg.end = end;
        }
        let changed = plan.changed;
        // 整轨重定时改的是 `segments`，必须置脏才会落库：`bump_segments_revision`
        // 只驱动清单缓存失效，不碰脏位；漏置脏时关窗守卫读到的 `segments_dirty`
        // 仍是 false，这次改动会被当作「无改动」直接丢弃。
        self.state.segments_dirty = true;
        self.state.bump_segments_revision();
        self.state.flush_segments_if_dirty();
        // 时间轴变了：波形/监视器的定位与质检缓存都要跟着重算。
        self.subtitle_filter_key = None;
        self.notice = Some(format!(
            "已调整整轨时间轴（{changed} 句变动{}）",
            if plan.overlaps {
                "，重叠已压平"
            } else {
                ""
            }
        ));
        cx.notify();
    }

    /// 把当前档位的「句间最小间隔」真正写回字幕。
    ///
    /// 写回口径与 [`MainWindow::apply_retime`] 逐条对齐：应用前记撤销快照 → 置脏 →
    /// `bump_segments_revision` → `flush_segments_if_dirty`，再清时间轴缓存键、给提示。
    /// 预览与写回共用同一份 `plan_min_gaps` 参数与同一个 [`min_gap_hint`]，不会出现
    /// 「预览说改 3 句、应用改了 4 句」。
    fn apply_min_gap(&mut self, cx: &mut Context<Self>) {
        use crate::subtitle::timing::plan_min_gaps;
        let op = self.retime_op;
        let gap_span = MIN_GAP_OP_BASE + MIN_GAP_PRESETS.len() as u8;
        if !(MIN_GAP_OP_BASE..gap_span).contains(&op) || self.state.segments.is_empty() {
            return;
        }
        let plan = plan_min_gaps(
            &self.state.segments,
            MIN_GAP_PRESETS[(op - MIN_GAP_OP_BASE) as usize],
        );
        // 无命中时不压撤销点：与 `plan_retime` 一样先看计划结果。两个计数都要看——
        // `changed` 只数起点被抬的句，「收短前句让后一句保住终点」只加 `shortened`，
        // 漏看它会把该做的改动当成无事发生，`Ctrl+Z` 又会白退一层。
        if plan.changed == 0 && plan.shortened == 0 {
            self.notice = Some(min_gap_hint(&plan, true));
            cx.notify();
            return;
        }
        // 回执文案先算好：`plan.times` 下面要被 move 走，之后不能再借 `plan` 取它。
        let notice_text = min_gap_hint(&plan, true);
        // 应用前记快照：一次可能挪动几十上百句，点错了必须有路可退。
        self.state.snapshot_for_undo();
        // 计划本身已保证「升序 + 相邻不重叠」，直接落地即可；**不再**过
        // `resolve_overlaps`——它会顺手把计划没打算动的过短片段补长，破坏
        // 「预览说改几句就改几句」的口径。
        for (seg, (start, end)) in self.state.segments.iter_mut().zip(plan.times) {
            seg.start = start;
            seg.end = end;
        }
        // 改的是 `segments`，必须置脏才会落库：`bump_segments_revision` 只驱动清单
        // 缓存失效，不碰脏位；漏置脏时关窗守卫读到的 `segments_dirty` 仍是 false，
        // 这次改动会被当作「无改动」直接丢弃。
        self.state.segments_dirty = true;
        self.state.bump_segments_revision();
        self.state.flush_segments_if_dirty();
        // 时间轴变了：波形/监视器的定位与质检缓存都要跟着重算。
        self.subtitle_filter_key = None;
        self.notice = Some(notice_text);
        cx.notify();
    }

    /// 字幕统计卡：整篇的时长 / 字数 / 语速 / 过长句。
    ///
    /// # 为什么这些数字值得单独一栏
    ///
    /// 「阅读速度（CPS）」与「单行过长」是字幕交付的两条硬指标——被平台或客户打回
    /// 的常见原因就是「这句字幕一闪而过看不清」。此前用户只能一行行目测，现在给出
    /// 整篇的极值与超标计数，并直接标出**是哪一句**（可点，跳过去改）。
    ///
    /// # 为什么按字符集分档
    ///
    /// CPS 对汉字与对英文字母不是一个量纲：一个汉字的信息量远大于一个字母。用同一个
    /// 阈值会把中文全判成「过快」或把英文全判成「舒适」，两头都不准。分档逻辑在
    /// `subtitle::stats::ReadingSpeed::classify`（纯函数、有单测），这里只负责展示。
    fn render_subtitle_stats_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::subtitle::stats::ReadingSpeed;
        let style = self.state.config.subtitle_style.clone();
        // 统计走派生快照：`compute` 每帧都要逐句 `count_visible` 扫全文（几千句时
        // 是这块里最贵的一笔），而结果只随字幕内容 / 媒体总时长 / 单行字数上限变，
        // 这三样都在快照的失效键里（媒体时长的取法与缓存键同源，见
        // `stats_media_secs`）。
        let stats = self.derived_snapshot().stats.clone();
        let max_cps_idx = stats.max_cps_index;
        let longest_idx = stats.longest_index;
        let too_fast = stats.too_fast_count;
        let long_lines = stats.long_line_count;

        // 统计行：标签 + 数值。`bad` 为真时用红色——只给需要用户动手的数字上色，
        // 全绿或全红都等于没有信号。
        let row = |label: &'static str, value: String, bad: bool| -> AnyElement {
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(Theme::SPACE_4))
                .py(px(Theme::SPACE_1))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_secondary())
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(Theme::TEXT_SMALL))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if bad {
                            Theme::accent_red()
                        } else {
                            Theme::text_primary()
                        })
                        .child(value),
                )
                .into_any_element()
        };

        let mut card = primitives::card_rows()
            .child(primitives::setting_row(
                "字幕统计",
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(format!("共 {} 句", stats.segment_count))
                    .into_any_element(),
            ))
            .child(primitives::divider());

        if stats.segment_count == 0 {
            return card.child(
                div()
                    .py(px(Theme::SPACE_2))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("还没有字幕，先在「智能转写」生成"),
            );
        }

        card = card
            .child(row(
                "原文总字数",
                format!("{} 字", stats.total_chars),
                false,
            ))
            .child(row(
                "已翻译",
                format!("{} / {} 句", stats.translated_count, stats.segment_count),
                false,
            ))
            .child(row(
                "有语音时长",
                format!(
                    "{}（占全片 {:.0}%）",
                    crate::utils::time::format_duration_short(stats.spoken_secs),
                    stats.spoken_ratio() * 100.0
                ),
                false,
            ))
            .child(row(
                "平均语速",
                format!("{:.1} 字/秒", stats.average_cps()),
                false,
            ))
            .child(row(
                "最快一句",
                format!("{:.1} 字/秒", stats.max_cps),
                stats
                    .max_cps_index
                    .and_then(|i| self.state.segments.iter().find(|s| s.index == i))
                    .and_then(|s| ReadingSpeed::classify(s.display_text(), s.duration()))
                    == Some(ReadingSpeed::TooFast),
            ));

        // 两个「有问题」的数字做成可点的胶囊：点一下跳到那一句，用户才知道去改哪。
        // 零项时保留计数但置灰——用户要看得出「这一项确实检查过、结果是 0」。
        // 跳转目标取极值句（语速过快 → 最快那句；单行过长 → 最长那句）：stats 只给
        // 极值，要精确到「第一句过长的」得再遍历一次，而极值句通常就是要改的那句。
        let mut issues = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2));

        if too_fast == 0 {
            issues = issues.child(primitives::tag_tinted(
                "语速过快 0 句",
                Theme::tint_neutral(),
                Theme::tint_neutral_border(),
                Theme::text_muted(),
            ));
        } else {
            let el = primitives::tag_tinted(
                format!("语速过快 {too_fast} 句"),
                Theme::tint_red_soft(),
                Theme::tint_red_border(),
                Theme::accent_red(),
            )
            .id("stats-issue-too-fast");
            issues = issues.child(match max_cps_idx {
                Some(idx) => el
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to_segment_for_stats(idx, cx);
                    }))
                    .into_any_element(),
                None => el.into_any_element(),
            });
        }

        if long_lines == 0 {
            issues = issues.child(primitives::tag_tinted(
                "单行过长 0 句",
                Theme::tint_neutral(),
                Theme::tint_neutral_border(),
                Theme::text_muted(),
            ));
        } else {
            let el = primitives::tag_tinted(
                format!("单行过长 {long_lines} 句"),
                Theme::tint_red_soft(),
                Theme::tint_red_border(),
                Theme::accent_red(),
            )
            .id("stats-issue-long-line");
            issues = issues.child(match longest_idx {
                Some(idx) => el
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to_segment_for_stats(idx, cx);
                    }))
                    .into_any_element(),
                None => el.into_any_element(),
            });
        }

        if let Some(idx) = longest_idx {
            issues = issues.child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(format!("最长句为第 {idx} 句（{} 字）", stats.max_chars)),
            );
        }

        card.child(issues).child(
            div()
                .pt(px(Theme::SPACE_2))
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(Theme::text_muted())
                .child(format!(
                    "语速上限按字符集分档：中日韩约 9 字/秒、拉丁约 17 字/秒；单行超过 {} 字判为过长（按可折两行算）",
                    style.max_chars_per_line.saturating_mul(2)
                )),
        )
    }

    /// 统计卡里的「跳到这一句」：选中目标句、强制字幕清单重新定位、刷新监视器帧。
    ///
    /// 与质检卡的 `jump_to_quality_issue` 同法但**不切页**——统计卡就在剪辑台里，
    /// 用户点它是想看一眼那一句，不是想离开当前页面。
    fn jump_to_segment_for_stats(&mut self, index: usize, cx: &mut Context<Self>) {
        self.state.select_segment(index);
        // 置 `None` 强制清单重新滚动到选中行：只在列表内部改选中不会自动跟随。
        self.subtitle_list_followed_sel = None;
        // 监视器要显示这一句对应的画面，否则用户还得自己在时间轴上找。
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 导出卡片：导出内容 / 文件名模板 / 编辑日志 / 格式选择 + 导出按钮。
    ///
    /// 它原本是**常驻在右侧面板最底部**的一条底栏（`render_editor_export_dock`），
    /// 无论切到哪个面板都占着约 150px 高。代价是那 150px 从右侧面板的可用高度里
    /// 切走，字幕清单 / 统计 / 翻译卡都因此少显示几行；而导出本身是**低频动作**，
    /// 不值得常驻。
    ///
    /// 现在收进「统计与导出」面板，成为该面板的第三张卡：三个面板各自占满整个
    /// 右侧高度，可用空间更宽裕、能多显示内容；导出就在同一页，要用时顺手可及。
    ///
    /// 下拉菜单放在触发行**之后**（向下展开）：本卡在可滚动面板里，向下展开比
    /// 向上顶出更符合阅读方向，也不会一开菜单就把上面的内容挤动。
    fn render_export_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self.is_export_dropdown_open;
        let cur_fmt = self.editor_export_format;
        let cur_mode = self.editor_export_mode;

        primitives::card_sm()
            .flex_none()
            .gap(px(Theme::SPACE_2))
            // 卡头：标题 + 本次会话改动数（原先挂在编辑日志那一排尾部）
            .child(primitives::setting_row(
                "导出与留档",
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(format!("本次会话 {} 条改动", self.state.edit_log.len()))
                    .into_any_element(),
            ))
            .child(primitives::divider())
            // 导出内容：原文 / 仅译文 / 双语。对字幕与剪辑工程文件统一生效。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .flex_shrink_0()
                            .child("导出内容"),
                    )
                    .child({
                        let modes = [
                            (ExportMode::RawOnly, "仅原文"),
                            (ExportMode::TranslationOnly, "仅译文"),
                            (ExportMode::Bilingual, "双语对照"),
                        ];
                        div()
                            .flex()
                            .gap_1()
                            .children(modes.into_iter().enumerate().map(|(idx, (mode, label))| {
                                let selected = mode == cur_mode;
                                primitives::chip_clickable(label, selected, false)
                                    .id(("export-mode-opt", idx))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.editor_export_mode = mode;
                                        // 落盘：下次启动仍用同一模式，避免「选项没生效」的错觉
                                        this.state.set_export_mode(mode);
                                        cx.notify();
                                    }))
                            }))
                    }),
            )
            // 文件名模板：`{name}` / `{ext}` / `{date}`。同一部片子常要交付多份
            // （给剪辑的、给外语同事的、按日期归档的），此前只能导出后手工改名。
            // 默认 `{name}.{ext}` = 历史行为，不动这个输入框的用户完全无感。
            .child({
                let tpl = self.state.config.ui.export_name_template.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .flex_shrink_0()
                            .child("文件名模板"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(self.render_export_name_template_input(cx)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .flex_shrink_0()
                            .child(if tpl.trim() == "{name}.{ext}" {
                                "占位符：{name} {ext} {date}"
                            } else {
                                "例：{name}.{date}.{ext}"
                            }),
                    )
            })
            // 编辑日志：与「导出字幕」同一排，因为它导出的是**这次编辑过程的记录**，
            // 属于「交付/留档」这一类动作。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .flex_shrink_0()
                            .child("编辑日志"),
                    )
                    .child(
                        primitives::chip_clickable("CSV", false, false)
                            .id("export-log-csv")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_edit_log(crate::subtitle::qc::QcFormat::Csv, cx);
                            })),
                    )
                    .child(
                        primitives::chip_clickable("Markdown", false, false)
                            .id("export-log-md")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_edit_log(crate::subtitle::qc::QcFormat::Markdown, cx);
                            })),
                    ),
            )
            // 格式选择 + 导出按钮
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .id("export-format-dropdown-trigger")
                            .flex_1()
                            .h(px(Theme::CTRL_H_LG))
                            .px(px(Theme::SPACE_4))
                            .rounded(px(Theme::RADIUS_LG))
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(if is_open {
                                Theme::accent_mint()
                            } else {
                                Theme::border()
                            })
                            .cursor_pointer()
                            .hover(|s| s.border_color(Theme::accent_mint()))
                            .flex()
                            .items_center()
                            .justify_between()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.is_export_dropdown_open = !this.is_export_dropdown_open;
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY_LG))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(cur_fmt.label()),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_secondary())
                                    .child(if is_open { "▲" } else { "▼" }),
                            ),
                    )
                    .child({
                        // 无字幕时置灰：导出链路（`perform_editor_export` 等）在没有
                        // segments 时都是静默 return，按钮却照常可点，用户会以为程序坏了。
                        // 同页的 mini_btn 早已有禁用态规范，导出按钮此前漏了这一层。
                        let can_export = !self.state.segments.is_empty();
                        primitives::btn_state(
                            "导出",
                            primitives::BtnSize::Lg,
                            primitives::BtnVariant::Primary,
                            can_export,
                        )
                        .id("editor-do-export-btn")
                        .px(px(Theme::SPACE_6))
                        .when(can_export, |d| {
                            d.on_click(cx.listener(|this, _, _, cx| {
                                this.is_export_dropdown_open = false;
                                this.perform_editor_export(cx);
                            }))
                        })
                    }),
            )
            // 格式下拉菜单（打开时）：放在触发行之后向下展开。
            .child(if is_open {
                div()
                    .id("export-format-menu")
                    .rounded_lg()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .p_1()
                    .max_h(px(Theme::DROPDOWN_MAX_H))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .children(
                        EditorExportFormat::all()
                            .iter()
                            .enumerate()
                            .map(|(idx, fmt)| {
                                let fmt = *fmt;
                                let is_selected = fmt == cur_fmt;
                                div()
                                    .id(("export-fmt-opt", idx))
                                    .px_2p5()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(if is_selected {
                                        Theme::tint_mint_badge()
                                    } else {
                                        Theme::transparent()
                                    })
                                    .hover(|s| s.bg(Theme::bg_hover()))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.editor_export_format = fmt;
                                        // 落盘：导出格式是长期偏好，不存的话重启就
                                        // 跳回默认，用户会以为选择没生效。
                                        this.state.config.ui.export_format =
                                            fmt.as_str().to_string();
                                        this.state.persist_config();
                                        this.is_export_dropdown_open = false;
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(if is_selected {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::NORMAL
                                            })
                                            .text_color(if is_selected {
                                                Theme::accent_mint()
                                            } else {
                                                Theme::text_primary()
                                            })
                                            .child(fmt.label()),
                                    )
                                    .child(if is_selected {
                                        div()
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child("[当前]")
                                    } else {
                                        div()
                                    })
                            }),
                    )
            } else {
                div().id("export-format-menu-closed")
            })
    }

    /// 查找替换面板（展开时才有内容；收起时返回空元素）。
    ///
    /// 真正的替换逻辑在 `subtitle::edit::replace_all`（纯函数、13 条单测）；这里只
    /// 负责输入、选项、执行与结果反馈。**不在这里拼任何字符串替换逻辑**——那正是
    /// 此前每个调用点各写一遍、大小写与译文保护各不相同的来源。
    ///
    /// 为什么替换前要记一次撤销快照：替换动辄改几十句，用户点错了必须有路可退。
    /// 走 `state.push_undo_snapshot()`（与所有编辑入口同一套），不另造机制。
    fn render_replace_bar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::subtitle::edit::{replace_all, ReplaceOptions, ReplaceScope, MAX_REPLACEMENTS};
        if !self.replace_panel_open {
            return None;
        }
        let find = self.replace_find.clone();
        let with = self.replace_with.clone();
        let case_sensitive = self.replace_case_sensitive;
        let include_translation = self.replace_include_translation;
        // 本会话的正则 / 整词 / 范围选择（见 `REPLACE_PANEL_TOGGLES`）。
        let (toggle_regex, toggle_whole_word, toggle_scope_selected) = REPLACE_PANEL_TOGGLES
            .with(|t| (t.regex.get(), t.whole_word.get(), t.scope_selected.get()));
        let status = self.replace_status.clone();
        let can_apply = !find.is_empty() && !self.state.segments.is_empty();
        // 规则集下拉要用的渲染数据：名字列表 + 当前选中项。只在面板展开时每帧克隆
        // 一次（量级是个位数），不值得再开一份缓存，也不会碰 `self` 的别名字段。
        let rule_names: Vec<String> = self
            .replace_rule_sets
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let rule_selected = self
            .replace_rule_selected
            .filter(|&pos| pos < rule_names.len());
        let rules_open = self.replace_rules_open;
        let rules_status = self.replace_rules_status.clone();
        let has_rules = !rule_names.is_empty();
        let has_selection = rule_selected.is_some();
        let has_segments = !self.state.segments.is_empty();
        let has_selected_segment = self.state.selected_segment_index.is_some();
        // 「存为规则集 / 追加到当前集」的可用性只看查找串：这两件事不依赖字幕表，
        // 空查找串存下来只会变成一条永不适用的规则。
        let has_find = !find.is_empty();

        // 两个自绘输入框：与 `render_api_input` 同一套最小可用编辑集合
        // （可打印字符 + 退格/方向/Home/End + Ctrl+A/C/V，见 `apply_line_edit`）。
        // 抽成闭包是因为两个框除了「绑哪个缓冲、哪个焦点」以外完全一样。
        let input = |id: &'static str,
                     field: crate::ui::ReplaceField,
                     value: &str,
                     placeholder: &str,
                     cx: &mut Context<Self>|
         -> AnyElement {
            use crate::ui::ReplaceField;
            let focus = match field {
                ReplaceField::Find => self.replace_find_focus.clone(),
                ReplaceField::With => self.replace_with_focus.clone(),
            };
            let is_focused = self.focused_replace_field == Some(field);
            let char_count = value.chars().count();
            let cursor = self.replace_cursor.min(char_count);
            let before: String = value.chars().take(cursor).collect();
            let after: String = value.chars().skip(cursor).collect();
            primitives::text_input(is_focused, 120.0)
                .id(id)
                .track_focus(&focus)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        window.focus(&focus);
                        this.focused_replace_field = Some(field);
                        this.replace_cursor = match field {
                            ReplaceField::Find => this.replace_find.chars().count(),
                            ReplaceField::With => this.replace_with.chars().count(),
                        };
                        cx.notify();
                    }),
                )
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        this.replace_panel_open = false;
                        this.replace_status = None;
                        cx.notify();
                        return;
                    }
                    let buffer = match field {
                        ReplaceField::Find => &mut this.replace_find,
                        ReplaceField::With => &mut this.replace_with,
                    };
                    let cursor = &mut this.replace_cursor;
                    if apply_line_edit(buffer, cursor, event, cx) {
                        cx.notify();
                    }
                }))
                .child(if is_focused {
                    div()
                        .flex()
                        .items_center()
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_primary())
                        .child(before)
                        .child(div().text_color(Theme::accent_blue()).child("▌"))
                        .child(after)
                        .into_any_element()
                } else if char_count == 0 {
                    div()
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_muted())
                        .truncate()
                        .child(placeholder.to_string())
                        .into_any_element()
                } else {
                    div()
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_primary())
                        .truncate()
                        .child(value.to_string())
                        .into_any_element()
                })
                .into_any_element()
        };

        let mut row = div()
            .w_full()
            .px(px(Theme::SPACE_3))
            .py(px(Theme::SPACE_2))
            .bg(Theme::bg_sidebar())
            .border_b_1()
            .border_color(Theme::border())
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("查找"),
            )
            .child(div().flex_1().min_w(px(120.0)).child(input(
                "replace-find-input",
                crate::ui::ReplaceField::Find,
                &find,
                "要查找的文字",
                cx,
            )))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("替换为"),
            )
            .child(div().flex_1().min_w(px(120.0)).child(input(
                "replace-with-input",
                crate::ui::ReplaceField::With,
                &with,
                "替换成（可留空＝删除）",
                cx,
            )));

        // 两个勾选：大小写、译文。用 mini_btn 的选中态表达，不引入新控件族。
        row = row
            .child(
                primitives::mini_btn(
                    if case_sensitive {
                        "✓ 区分大小写"
                    } else {
                        "区分大小写"
                    },
                    true,
                )
                .id("replace-toggle-case")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.replace_case_sensitive = !this.replace_case_sensitive;
                    cx.notify();
                })),
            )
            .child(
                primitives::mini_btn(
                    if include_translation {
                        "✓ 含译文"
                    } else {
                        "含译文"
                    },
                    true,
                )
                .id("replace-toggle-translation")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.replace_include_translation = !this.replace_include_translation;
                    cx.notify();
                })),
            )
            .child(
                primitives::mini_btn(if toggle_regex { "✓ 正则" } else { "正则" }, true)
                    .id("replace-toggle-regex")
                    .on_click(cx.listener(|_this, _, _, cx| {
                        REPLACE_PANEL_TOGGLES.with(|t| t.regex.set(!t.regex.get()));
                        cx.notify();
                    })),
            )
            .child(
                primitives::mini_btn(
                    if toggle_whole_word {
                        "✓ 整词"
                    } else {
                        "整词"
                    },
                    true,
                )
                .id("replace-toggle-whole-word")
                .on_click(cx.listener(|_this, _, _, cx| {
                    REPLACE_PANEL_TOGGLES.with(|t| t.whole_word.set(!t.whole_word.get()));
                    cx.notify();
                })),
            )
            .child(
                primitives::mini_btn(
                    if toggle_scope_selected {
                        "全部句"
                    } else {
                        "✓ 全部句"
                    },
                    true,
                )
                .id("replace-scope-all")
                .on_click(cx.listener(|_this, _, _, cx| {
                    REPLACE_PANEL_TOGGLES.with(|t| t.scope_selected.set(false));
                    cx.notify();
                })),
            )
            .child(
                primitives::mini_btn(
                    if toggle_scope_selected {
                        "✓ 仅当前句"
                    } else {
                        "仅当前句"
                    },
                    has_selected_segment,
                )
                .id("replace-scope-selected")
                .when(has_selected_segment, |d| {
                    d.on_click(cx.listener(|_this, _, _, cx| {
                        REPLACE_PANEL_TOGGLES.with(|t| t.scope_selected.set(true));
                        cx.notify();
                    }))
                }),
            )
            .child(
                primitives::mini_btn("全部替换", can_apply)
                    .id("replace-apply")
                    .when(can_apply, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            // 范围=仅当前句但此刻没有选中句时：只提示、绝不动全部——
                            // 否则「只想改一句」会变成整轨批量替换，方向正好反了。
                            let scope_selected =
                                REPLACE_PANEL_TOGGLES.with(|t| t.scope_selected.get());
                            let scope = match (scope_selected, this.state.selected_segment_index) {
                                (true, Some(index)) => ReplaceScope::SelectedSentence(index),
                                (true, None) => {
                                    this.replace_status = Some((false, "未选中句子".to_string()));
                                    cx.notify();
                                    return;
                                }
                                (false, _) => ReplaceScope::All,
                            };
                            // 先记快照：替换可能一次改几十句，点错必须有路可退。
                            this.state.snapshot_for_undo();
                            let options = ReplaceOptions {
                                case_sensitive: this.replace_case_sensitive,
                                include_translation: this.replace_include_translation,
                                regex: REPLACE_PANEL_TOGGLES.with(|t| t.regex.get()),
                                whole_word: REPLACE_PANEL_TOGGLES.with(|t| t.whole_word.get()),
                                scope,
                            };
                            let report = replace_all(
                                &mut this.state.segments,
                                &this.replace_find,
                                &this.replace_with,
                                &options,
                            );
                            this.replace_status =
                                Some(if let Some(err) = report.regex_error.as_deref() {
                                    (false, format!("正则表达式无效：{err}"))
                                } else if report.is_empty() {
                                    (false, "没有找到匹配的内容".to_string())
                                } else {
                                    let mut msg = format!(
                                        "已替换 {} 处，涉及 {} 句",
                                        report.total_hits(),
                                        report.changed.len()
                                    );
                                    if report.translation_hits > 0 {
                                        msg.push_str(&format!(
                                            "（其中译文 {} 处）",
                                            report.translation_hits
                                        ));
                                    }
                                    if report.truncated {
                                        msg.push_str(&format!(
                                            "；命中 {} 处，已达单次上限 {}，其余未替换",
                                            report.matched_hits, MAX_REPLACEMENTS
                                        ));
                                    }
                                    (true, msg)
                                });
                            // 内容变了：字幕清单的过滤结果与质检缓存都要跟着重算。
                            // 同时置脏并落库：`bump_segments_revision` 只失效清单缓存，
                            // 不置脏的话这次整轨替换不会被写回历史库。
                            this.state.segments_dirty = true;
                            this.state.bump_segments_revision();
                            this.state.flush_segments_if_dirty();
                            this.subtitle_filter_key = None;
                            cx.notify();
                        }))
                    }),
            )
            .child(
                primitives::mini_btn("关闭", true)
                    .id("replace-close")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.replace_panel_open = false;
                        this.replace_status = None;
                        cx.notify();
                    })),
            );

        if let Some((ok, msg)) = status {
            row = row.child(
                div()
                    .flex_basis(relative(1.0))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if ok {
                        Theme::accent_mint()
                    } else {
                        Theme::accent_orange()
                    })
                    .child(msg),
            );
        }

        // ── 规则集（第 11 项）──
        //
        // 独立一行：上面那一行的控件顺序、间距、折行行为都不动，只是在它下面多挂
        // 一条同款外壳的行。「应用到全部 / 应用到选中句」沿用上面两个勾选
        // （大小写 / 含译文）作为匹配口径，用户在同一面板里就能看清会怎么匹配。
        let mut rules_row = div()
            .w_full()
            .px(px(Theme::SPACE_3))
            .py(px(Theme::SPACE_2))
            .bg(Theme::bg_sidebar())
            .border_b_1()
            .border_color(Theme::border())
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("规则集"),
            )
            .child(
                primitives::mini_btn(
                    if rules_open {
                        "收起 ▴"
                    } else {
                        "选择 ▾"
                    },
                    true,
                )
                .id("replace-rules-toggle")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.replace_rules_open = !this.replace_rules_open;
                    cx.notify();
                })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .child(match rule_selected {
                        Some(pos) => rule_names[pos].clone(),
                        None if has_rules => "未选择".to_string(),
                        None => {
                            "（还没有规则集，可点「导入」或先用「全部替换」跑一次）".to_string()
                        }
                    }),
            );

        // 展开时逐个列出（选中项带 ✓）：只有个位数条目，不必再做虚拟列表。
        if rules_open {
            for (pos, name) in rule_names.iter().enumerate() {
                let label = if Some(pos) == rule_selected {
                    format!("✓ {name}")
                } else {
                    name.clone()
                };
                rules_row = rules_row.child(
                    primitives::mini_btn(label, true)
                        .id(("replace-rule-set", pos))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.replace_rule_selected = Some(pos);
                            this.replace_rules_open = false;
                            cx.notify();
                        })),
                );
            }
        }

        rules_row = rules_row
            .child(
                primitives::mini_btn("应用到全部", has_selection && has_segments)
                    .id("replace-rules-apply-all")
                    .when(has_selection && has_segments, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.apply_selected_replace_rule_set(ReplaceRuleScope::AllSegments, cx);
                        }))
                    }),
            )
            .child(
                primitives::mini_btn("应用到选中句", has_selection && has_selected_segment)
                    .id("replace-rules-apply-one")
                    .when(has_selection && has_selected_segment, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.apply_selected_replace_rule_set(
                                ReplaceRuleScope::SelectedSentence,
                                cx,
                            );
                        }))
                    }),
            )
            .child(
                primitives::mini_btn("保存", true)
                    .id("replace-rules-save")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_replace_rule_sets_now(cx);
                    })),
            )
            .child(
                primitives::mini_btn("删除当前集", has_selection)
                    .id("replace-rules-delete")
                    .when(has_selection, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.delete_selected_replace_rule_set(cx);
                        }))
                    }),
            )
            .child(
                primitives::mini_btn("导入", true)
                    .id("replace-rules-import")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_replace_rule_sets(cx);
                    })),
            )
            .child(
                primitives::mini_btn("导出", has_rules)
                    .id("replace-rules-export")
                    .when(has_rules, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.export_replace_rule_sets(cx);
                        }))
                    }),
            )
            // 「增」的两个入口：新建一个集合 / 往当前集合里加一条。没有它们，
            // 规则集只能靠导入 JSON 或手写文件，面板里试好的替换就留不下来。
            .child(
                primitives::mini_btn("存为规则集", has_find)
                    .id("replace-rules-save-current")
                    .when(has_find, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.save_current_replace_as_rule_set(cx);
                        }))
                    }),
            )
            .child(
                primitives::mini_btn("追加到当前集", has_find && has_selection)
                    .id("replace-rules-append-current")
                    .when(has_find && has_selection, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.append_current_replace_rule(cx);
                        }))
                    }),
            );

        if let Some((ok, msg)) = rules_status {
            rules_row = rules_row.child(
                div()
                    .flex_basis(relative(1.0))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if ok {
                        Theme::accent_mint()
                    } else {
                        Theme::accent_orange()
                    })
                    .child(msg),
            );
        }

        // 外层只是把「原有的那一行」与「新的规则集行」摞起来：原行的元素、属性、
        // 子节点顺序一个没动。
        Some(
            div()
                .w_full()
                .flex()
                .flex_col()
                .child(row)
                .child(rules_row)
                .into_any_element(),
        )
    }

    /// 导出文件名模板输入框（走与 API 输入框同一套最小编辑集合）。
    ///
    /// 校验**故意宽松**：模板是自由文本，任何「看起来不对」的值都仍能导出（最差
    /// 只是名字不合心意），为此弹红字只会打扰。真正的兜底在
    /// `writer::export_file_name`：空模板回落默认、无占位符时补扩展名。
    fn render_export_name_template_input(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.export_template_focus.clone();
        let is_focused = self.export_template_focused;
        let value = self.state.config.ui.export_name_template.clone();
        let char_count = value.chars().count();
        let cursor = self.export_template_cursor.min(char_count);
        let before: String = value.chars().take(cursor).collect();
        let after: String = value.chars().skip(cursor).collect();
        primitives::text_input(is_focused, 120.0)
            .id("export-name-template-input")
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    window.focus(&focus);
                    this.export_template_cursor =
                        this.state.config.ui.export_name_template.chars().count();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let buffer = &mut this.state.config.ui.export_name_template;
                let cursor = &mut this.export_template_cursor;
                if apply_line_edit(buffer, cursor, event, cx) {
                    this.state.persist_config();
                    cx.notify();
                }
            }))
            .child(if is_focused {
                div()
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .child(before)
                    .child(div().text_color(Theme::accent_blue()).child("▌"))
                    .child(after)
                    .into_any_element()
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child("{name}.{ext}")
                    .into_any_element()
            } else {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .truncate()
                    .child(value)
                    .into_any_element()
            })
            .into_any_element()
    }

    /// 字幕清单的搜索输入框。
    ///
    /// Esc 一键清空（比按住退格删十个字快得多），其余按键复用 [`apply_line_edit`]
    /// 的最小可用编辑集合。搜索结果是渲染时按需重算的，这里只需改缓冲并重绘。
    fn render_search_input(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let is_focused = self.subtitle_search_focused;
        let focus = self.subtitle_search_focus.clone();
        let raw = self.subtitle_search.clone();
        let char_count = raw.chars().count();
        let cursor = self.subtitle_search_cursor.min(char_count);
        let before: String = raw.chars().take(cursor).collect();
        let after: String = raw.chars().skip(cursor).collect();

        div()
            .id("subtitle-search-input")
            .flex_1()
            .min_w(px(Theme::SEARCH_MIN_W))
            .h(px(Theme::CTRL_H_XS))
            .px(px(Theme::SPACE_2))
            .rounded(px(Theme::RADIUS_MD))
            .bg(Theme::bg_input())
            .border_1()
            .border_color(if is_focused {
                Theme::accent_mint()
            } else {
                Theme::border_mid()
            })
            .cursor_text()
            .flex()
            .items_center()
            .overflow_hidden()
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    window.focus(&focus);
                    this.subtitle_search_focused = true;
                    this.subtitle_search_cursor = this.subtitle_search.chars().count();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.subtitle_search.clear();
                    this.subtitle_search_cursor = 0;
                    cx.notify();
                    return;
                }
                let buffer = &mut this.subtitle_search;
                let cursor = &mut this.subtitle_search_cursor;
                if apply_line_edit(buffer, cursor, event, cx) {
                    cx.notify();
                }
            }))
            .child(if is_focused {
                div()
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .child(before)
                    .child(div().text_color(Theme::accent_mint()).child("▌"))
                    .child(after)
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("输入关键字过滤原文或译文，Esc 清空")
            } else {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .truncate()
                    .child(raw)
            })
            .into_any_element()
    }

    /// 单选胶囊（翻译引擎档位 / 目标语言共用）。
    fn render_choice_pill(
        id: (&'static str, usize),
        label: &str,
        is_sel: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        primitives::chip_clickable(label.to_string(), is_sel, false)
            .id(id)
            .on_click(cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
                cx.notify();
            }))
    }

    /// 字幕多语言翻译面板：引擎档位、目标语言、进度与一键翻译。
    ///
    /// 翻译链路（本地 Qwen / 在线 OpenAI 兼容 API）与 `AppState` 里的进度字段
    /// 此前都已就绪，缺的只是这个入口——没有它，「翻译字幕」列永远是「—」。
    fn render_translate_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::app::state::TRANSLATE_TARGET_LANGS;
        use crate::engines::TranslateMode;

        let mode = self.state.translate_mode;
        let target = self.state.translate_target_lang.clone();
        let is_translating = self.state.is_translating;
        let progress = self.state.translate_progress.clamp(0.0, 1.0);
        let status = self.state.translate_status_msg.clone();
        // 下面五项（译出 / 分母 / 其他语言 / 有无译文 / 是否双语）都从派生快照取：
        // 它们过去各自遍历一遍全表，而播放时每 40ms 就要重绘一次。
        let snapshot = self.derived_snapshot();
        // 「已完成」只认**实际译出**：`has_translation()` 要求译文非空（忽略空串/纯空白），
        // 再叠加目标语言匹配。只要还有译文为空/纯空白（哪怕引擎给它打了
        // `translation_lang`），这里就不会凑满 total，界面绝不会显示「已全部翻译」。
        let done = snapshot.translated_out;
        // 分母只算**源文非空**的句子（与引擎的可译口径、与收尾文案一致）：
        // 空源句（静音段/纯空白行）永远不会有译文，计进分母就会永远差几句。
        let expected = snapshot.expected_translation;
        let total = self.state.segments.len();
        // 可点的前提是**有可翻译的文本**：整片都是空源句时点下去只会
        // 得到「没有可翻译的文本」，不如直接置灰。
        let can_run = expected > 0 && !is_translating;
        // 已带译文、但**不是**当前目标语言的句数。
        // 有了它才能解释「为什么按钮不是『重新翻译』」：用户切换目标语言后
        // 旧译文仍然在，但当前语言一句都没有——不提示的话，界面看起来像
        // 「翻译记录丢了」，用户会以为程序把之前的成果清空了。
        let other_lang = snapshot.other_lang_translated;
        let has_any_trans_overall = snapshot.has_any_translation;
        let has_distinct_trans = snapshot.has_distinct_translation;
        // 「译文与原文一字不差」的句子存在与否：决定要不要给出「清理复制译文」入口。
        // 取派生快照（按修订号重建一次），不在渲染路径上现扫全表。
        let has_copied_trans = snapshot.has_copied_translation;

        // 引擎档位：本地 Qwen 免费离线，在线 API 更快更好但需要密钥
        let mode_row = div()
            .flex()
            .items_center()
            .gap_1p5()
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(Theme::FORM_LABEL_W))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_secondary())
                    .child("翻译引擎"),
            )
            .child(Self::render_choice_pill(
                ("translate-mode", 0),
                TranslateMode::OfflineQwen.label(),
                mode == TranslateMode::OfflineQwen,
                cx,
                |this, cx| {
                    this.state
                        .set_translate_mode(crate::engines::TranslateMode::OfflineQwen);
                    cx.notify();
                },
            ))
            .child(Self::render_choice_pill(
                ("translate-mode", 1),
                TranslateMode::OnlineApi.label(),
                mode == TranslateMode::OnlineApi,
                cx,
                |this, cx| {
                    this.state
                        .set_translate_mode(crate::engines::TranslateMode::OnlineApi);
                    cx.notify();
                },
            ))
            .child(
                div()
                    .flex_1()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child(if mode == TranslateMode::OnlineApi {
                        let model = self.state.config.translate.api_model.clone();
                        if self
                            .state
                            .config
                            .translate
                            .effective_api_key()
                            .trim()
                            .is_empty()
                        {
                            "未配置 API Key，请在「性能设置」中填写".to_string()
                        } else {
                            format!("模型 {}", model)
                        }
                    } else {
                        // 离线链路需要两件东西：Qwen 模型与 llama.cpp 推理程序。
                        // 缺哪个就点名哪个，避免用户点「开始翻译」后才收到错误；
                        // 两者均缺时先提模型（体积大、下载慢，先知道更有心理预期）。
                        // 模型路径可能是用户自备的（性能设置里可浏览 / 粘贴），
                        // 因此文案不能只说「未下载」——自备路径填错时同样会命中这里，
                        // 说「未就位」并给出两条出路（去模型页下载 / 去性能设置核对路径）
                        // 才覆盖得住。
                        let miss_model = !self.state.model_is_present("qwen-llm");
                        let miss_cli = !self.state.model_is_present("llama-cpp");
                        if miss_model {
                            "本地模型未就位，请在「性能设置」核对路径或到「模型与组件」下载"
                                .to_string()
                        } else if miss_cli {
                            "未下载 llama.cpp 推理程序，请在「性能设置 → 模型与组件」下载"
                                .to_string()
                        } else {
                            "本地 Qwen，无需密钥".to_string()
                        }
                    }),
            );

        // 目标语言：8 个常用语种，窄面板下自动折行
        let lang_row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_1()
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(Theme::FORM_LABEL_W))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_secondary())
                    .child("目标语言"),
            )
            .children(TRANSLATE_TARGET_LANGS.iter().enumerate().map(|(i, lang)| {
                let is_sel = target == *lang;
                let lang_owned = lang.to_string();
                Self::render_choice_pill(
                    ("translate-lang", i),
                    lang,
                    is_sel,
                    cx,
                    move |this, cx| {
                        // 走 setter 而非直接赋值：目标语言是长期偏好，必须落盘，
                        // 否则重启后跳回默认语言，用户会以为选择没生效。
                        this.state.set_translate_target_lang(&lang_owned);
                        cx.notify();
                    },
                )
            }));

        let action_row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            // 机翻难免有出入：选中某句后可直接订正译文（走系统输入框，支持中文输入法）。
            // 放在翻译卡里，是因为用户就是在这一面板发现某句译得不对。
            .child(if self.state.selected_segment_index.is_some() {
                primitives::btn_clickable(
                    "改译文",
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id("btn-translate-edit-selected")
                .on_click(cx.listener(|this, _, window, cx| {
                    if let Some(idx) = this.state.selected_segment_index {
                        let trans = this.state.segments.iter().find(|s| s.index == idx)
                            .and_then(|s| s.translation.clone())
                            .unwrap_or_default();
                        this.start_inline_edit(idx, true, &trans, window, cx);
                    }
                }))
                .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(
                div()
                    .id("btn-translate-subtitles")
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(if can_run {
                        Theme::accent_mint()
                    } else {
                        Theme::bg_disabled()
                    })
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if can_run {
                        Theme::text_on_accent()
                    } else {
                        Theme::text_disabled()
                    })
                    .when(can_run, |s| {
                        s.cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.trigger_llm_translation(cx);
                            }))
                    })
                    .child(if is_translating {
                        "翻译中…".to_string()
                    } else if expected > 0 && done == expected {
                        // 当前语言应译的句子全部译出（`done` 只数**实际译出**的句子，
                        // 空/纯空白译文永远凑不满）：再点只会被引擎的增量
                        // 逻辑判为「无需翻译」，文案说清楚，避免用户以为按钮失灵。
                        "已全部翻译".to_string()
                    } else if done > 0 {
                        // 部分完成（含取消后继续、或补译新句）——增量翻译，
                        // 不会把已完成的部分重译一遍。
                        "继续翻译".to_string()
                    } else if other_lang > 0 {
                        // 有别的语言的译文、当前语言一句都没有：这是「换语言」场景，
                        // 按钮要说清是「译成新语言」而不是「重新翻译」。
                        format!("译为{}", target)
                    } else {
                        "开始翻译".to_string()
                    }),
            )
            // 翻译中才出现的「取消」：离线 Qwen 模型路径不对、在线 API 长时间无响应时，
            // 此前只能强杀进程——按钮变成灰的「翻译中…」且不可点，是条纯死路。
            // 取消只需置位一个 AtomicBool，引擎在**每个批次之间**检查，最多损失当前批。
            .child(if is_translating {
                primitives::btn_clickable(
                    "取消",
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id("btn-translate-cancel")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cancel_llm_translation(cx);
                }))
                .into_any_element()
            } else {
                div().into_any_element()
            })
            // 存在译文且未在翻译中时，提供一键清空译文入口，方便恢复纯净单语状态
            .child(if has_any_trans_overall && !is_translating {
                primitives::btn_clickable(
                    "清空译文",
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id("btn-translate-clear-all")
                .on_click(cx.listener(|this, _, _, cx| {
                    // 这里**不再**手工补 `bump_segments_revision`：清空译文的实现
                    // （`actions::clear_translations_with_undo`）自己会递增修订号，
                    // 失效责任收在改动点上——此前靠这一行补偿，任何新调用点漏补一次
                    // 都会让列表继续按双语行高渲染、翻译卡仍说「已全部翻译」。
                    this.clear_all_translations(cx);
                }))
                .into_any_element()
            } else {
                div().into_any_element()
            })
            // 只清「译文与原文一字不差」的句子：本地小模型摆烂时整份译文里往往只有
            // 这几句是废的，一键全清会把已逐句校对好的译文一起扔掉。
            .child(if has_copied_trans && !is_translating {
                primitives::btn_clickable(
                    "清理复制译文",
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id("btn-translate-clear-copied")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.clear_copied_translations(cx);
                }))
                .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if status.contains("失败") {
                        Theme::accent_red()
                    } else {
                        Theme::text_muted()
                    })
                    .truncate()
                    .child(if status.is_empty() {
                        if total == 0 {
                            "先在「智能转写」生成字幕，再回到这里翻译".to_string()
                        } else if expected > 0 && done == expected {
                            // 同上：`done` 只认实际译出，空/纯空白译文不会被
                            // 当成「已全部翻译为{target}」。
                            if !has_distinct_trans && has_any_trans_overall {
                                "已处理完毕（译文同原文，画面与列表已自动单行显示；若无需译文可点「清空译文」）".to_string()
                            } else {
                                format!("已全部翻译为{target}")
                            }
                        } else if expected == 0 {
                            // 有字幕但没有任何非空源文：没有可翻译的内容
                            "所有字幕行都为空，没有可翻译的文本".to_string()
                        } else if other_lang > 0 && done == 0 {
                            // 关键提示：换语言后旧译文仍在，只是不属于当前目标语言。
                            // 不说明的话，界面看起来像翻译记录被清空了。
                            format!("已有 {other_lang} 句其他语言译文；点上方按钮可译成{target}")
                        } else if done > 0 {
                            format!("{target}译文 {done}/{expected} 句，可继续补译")
                        } else {
                            format!("尚未翻译（共 {expected} 句）")
                        }
                    } else {
                        status
                    }),
            );

        primitives::card_sm()
            .flex_none()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child("字幕翻译"),
                    )
                    .child(
                        // 计数徽标走 primitives::count_tag，仅按完成量覆盖文字色。
                        // 分母用**应有译文句数**（排除空源句），否则与收尾文案不一致。
                        primitives::count_tag(format!("{}/{} 句已翻译", done, expected)).text_color(
                            if done > 0 {
                                Theme::accent_mint()
                            } else {
                                Theme::text_secondary()
                            },
                        ),
                    ),
            )
            .child(mode_row)
            .child(lang_row)
            .child(action_row)
            // 术语表状态 + 快速入口：翻译就在这一面板发生，把「固定译法」的入口放在这里
            // 最顺手。点击直接跳到设置页的术语表编辑（打开系统编辑器改）。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_secondary())
                            .child("术语表"),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child({
                                let n = self.state.glossary_entries().len();
                                if n == 0 {
                                    "未设置".to_string()
                                } else {
                                    // 显示**实际生效**条数：`glossary_prompt` 只注入前
                                    // `MAX_GLOSSARY_ENTRIES` 条，超过的部分静默丢弃，
                                    // 不提示的话用户会以为 120 条全在生效。
                                    let limit =
                                        self.state.config.translate.effective_glossary_limit();
                                    let effective =
                                        super::views::performance::effective_glossary_count(n, limit);
                                    let cap_note = if effective < n {
                                        format!(
                                            "，其中 {effective} 条生效（受上限 {limit} 条限制）"
                                        )
                                    } else {
                                        String::new()
                                    };
                                    let bad = self.cached_glossary_violations().len();
                                    if bad > 0 {
                                        format!(
                                            "已启用 {n} 条{cap_note} · {bad} 句疑似未按术语译（见琥珀色行）"
                                        )
                                    } else {
                                        format!("已启用 {n} 条{cap_note}（专名/术语按固定译法）")
                                    }
                                }
                            }),
                    )
                    .child(
                        primitives::chip_clickable("编辑术语表", false, false)
                            .id("editor-glossary-edit")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_glossary_editor(cx);
                            })),
                    )
                    .child(
                        primitives::chip_clickable("应用", false, false)
                            .id("editor-glossary-apply")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.apply_glossary_from_file(cx);
                            })),
                    ),
            )
            // 翻译进行中才出现进度条，空闲时不占版面
            .child(if is_translating {
                div()
                    .w_full()
                    .h(px(Theme::PROGRESS_H))
                    .rounded_full()
                    .bg(Theme::bg_inset())
                    .border_1()
                    .border_color(Theme::border())
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .w(relative(progress))
                            .bg(Theme::accent_mint()),
                    )
            } else {
                div()
            })
    }

    /// 属性面板里的紧凑次级按钮（拆分 / 合并 / 时间微调共用同一套外观）。
    ///
    /// `enabled = false` 时置灰且不响应点击，用于「最后一句无法合并」这类
    /// 点了也没反应的操作，避免用户误以为程序坏了。
    fn render_mini_btn(
        id: &'static str,
        label: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let base = primitives::mini_btn(label, enabled).id(id);
        if enabled {
            base.on_click(cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
                cx.notify();
            }))
        } else {
            base
        }
    }

    /// 标点快捷注入按钮 (iOS Pill Chip)
    fn render_punct_btn(
        &mut self,
        punct: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        primitives::chip_clickable(punct, false, false)
            .id(punct)
            .text_color(Theme::text_primary())
            .on_click(cx.listener(move |this, _, _, cx| {
                let total_chars = this.state.editing_text.chars().count();
                let cursor = this.text_cursor_pos.min(total_chars);
                let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                let punct_chars: Vec<char> = punct.chars().collect();
                let p_len = punct_chars.len();
                chars.splice(cursor..cursor, punct_chars);
                this.state.editing_text = chars.into_iter().collect();
                this.text_cursor_pos = cursor + p_len;
                this.state.save_selected_text();
                cx.notify();
            }))
            .child(punct)
    }
}

// 时间轴的水平平移量（可视窗口起点，秒）。
//
// 不挂在 `MainWindow` 上：窗口结构体定义在 `ui/mod.rs`，本模块不便向外扩字段
// （同 `REPLACE_PANEL_TOGGLES`）。缩放复用已有字段 `state.timeline_zoom`，平移是纯
// 视图态——不需要持久化，换了素材后 `clamp_window` 会把它夹回新素材范围内。
thread_local! {
    static TIMELINE_PAN_START: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

// 「用户手动调过视图」标记：记的是手动平移/缩放**那一刻的播放头时间**（秒）。
//
// 暂停时窗口不跟随播放头（用户手动平移的意图优先），但只有在用户没动过视图时才做
// 「播放头离屏就拉回」的兜底；播放头一动（seek / 播放推进）该标记自动失效，跟随随之
// 恢复——所以从清单点一句跳到窗口外的句子仍会把窗口带过去。
thread_local! {
    static TIMELINE_PAN_ANCHOR: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
}

/// 时间轴缩放上下限：1.0 = 整段（与改造前一致），20.0 = 一秒的素材也能占到 1/20 宽。
const TIMELINE_ZOOM_MIN: f64 = 1.0;
const TIMELINE_ZOOM_MAX: f64 = 20.0;
/// 顶栏 `-` / `+` 一次点击的倍率，以及滚轮一格的倍率。
const TIMELINE_ZOOM_BTN_STEP: f64 = 1.5;
const TIMELINE_ZOOM_SCROLL_STEP: f64 = 1.15;
/// 滚轮一格的水平平移量（占窗口宽度的比例）。
const TIMELINE_PAN_STEP: f64 = 0.1;
/// 播放跟随可视带：播放中游标进入前后各 10% 就把窗口推着走，留出前视空间。
const TIMELINE_FOLLOW_BAND: f64 = 0.1;

/// 时间轴的可视时间窗口 `[start, end]`，是整个时间轴**唯一**的「时间 → 横向比例」变换。
///
/// 为什么收敛成一个类型：缩放与平移一旦分别内联进片段、播放头、标尺、波形各自的
/// 表达式，就必然出现「有的按整段、有的按窗口」的错位——那正是这次要根治的问题。
#[derive(Clone, Copy, Debug, PartialEq)]
struct TimelineWindow {
    start: f64,
    end: f64,
}

impl TimelineWindow {
    /// 时间 → 窗口内比例。**不夹紧**：落在窗口外的时间要拿到 < 0 或 > 1 的值，片段
    /// 据此被容器的 `overflow_hidden` 裁掉，而不是全挤在边缘。
    fn time_to_ratio(self, t: f64) -> f64 {
        let span = self.span();
        if span <= 0.0 {
            return 0.0;
        }
        (t - self.start) / span
    }

    /// 窗口内比例 → 时间。比例夹到 `[0, 1]`：鼠标拖出轨道时只取端点时间。
    fn ratio_to_time(self, ratio: f64) -> f64 {
        self.start + ratio.clamp(0.0, 1.0) * self.span()
    }

    fn span(self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}

/// 倍率夹到 `[MIN, MAX]`；非有限值退回整段，避免 NaN 渗进布局。
fn clamp_zoom(zoom: f64) -> f64 {
    if !zoom.is_finite() {
        return TIMELINE_ZOOM_MIN;
    }
    zoom.clamp(TIMELINE_ZOOM_MIN, TIMELINE_ZOOM_MAX)
}

/// 把窗口夹回 `[0, total]`：平移不越界、缩放后不越界，总时长为 0 时退化成空窗口。
///
/// `span >= total`（整段或更宽）一律回到 `[0, total]`——这正是「zoom = 1.0 与改造前
/// 逐像素一致」的根：未放大时任何平移/跟随都会被这里抹掉。
fn clamp_window(start: f64, end: f64, total: f64) -> (f64, f64) {
    if !total.is_finite() || total <= 0.0 {
        return (0.0, 0.0);
    }
    let span = if start.is_finite() && end.is_finite() {
        (end - start).max(0.0)
    } else {
        total
    };
    if span >= total {
        return (0.0, total);
    }
    let start = if start.is_finite() {
        start.clamp(0.0, total - span)
    } else {
        0.0
    };
    (start, start + span)
}

/// 由「倍率 + 窗口起点」导出夹紧后的窗口：状态 → 变换的唯一出口，渲染、鼠标定位、
/// 播放跟随都走这里，不各算各的。
fn timeline_window(zoom: f64, start: f64, total: f64) -> TimelineWindow {
    let zoom = clamp_zoom(zoom);
    if !total.is_finite() || total <= 0.0 {
        return TimelineWindow {
            start: 0.0,
            end: 0.0,
        };
    }
    let span = total / zoom;
    let (start, end) = clamp_window(start, start + span, total);
    TimelineWindow { start, end }
}

/// 以 `anchor_ratio`（鼠标在时间轴上的横向占比）为不动点缩放。
///
/// 锚点不动是缩放的语义要求：鼠标指着哪一帧，滚完之后还得指着那一帧，否则每滚一次
/// 都要重新找位置。返回 `(夹紧后的倍率, 夹紧后的窗口起点)`。
fn zoom_at(
    anchor_ratio: f64,
    visible_start: f64,
    total: f64,
    zoom: f64,
    factor: f64,
) -> (f64, f64) {
    let zoom = clamp_zoom(zoom);
    if !total.is_finite() || total <= 0.0 || !factor.is_finite() || factor <= 0.0 {
        return (zoom, 0.0);
    }
    let anchor = if anchor_ratio.is_finite() {
        anchor_ratio.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let anchor_time = visible_start + anchor * (total / zoom);
    let new_zoom = clamp_zoom(zoom * factor);
    let span = total / new_zoom;
    let start = anchor_time - anchor * span;
    let (start, _) = clamp_window(start, start + span, total);
    (new_zoom, start)
}

/// 播放头跟随：窗口跟着游标走，保证「按了播放一定看得见游标」。
///
/// **播放中**按前后各 [`TIMELINE_FOLLOW_BAND`] 的可视带跟随（游标不贴死边缘，留出前视
/// 空间），并把游标推到窗口内 90% / 10% 的位置。
///
/// **暂停时不跟随**——这是修正过的语义。旧实现把「暂停」写成 `band = 0` 的同一条分支，
/// 看着只是取消了前后留白，其实还留着「窗口必须包住播放头」这条硬约束：只要手动平移把
/// 播放头挤出窗口，下一帧就被拉回来（`current_time` 初值 0.0 贴在左缘，于是向右平移等于
/// 白滚，滚轮/平移完全没反应）。现在暂停时只在用户**没动过视图**时才保留一个兜底
/// （播放头整个出窗口就拉回），动过视图就一律保持用户给的窗口，播放头允许离屏，
/// 直到一次 seek / 播放把它带回来。
///
/// `manual_anchor` 就是「动过视图」的标记：内容是手动调整视图那一刻的播放头时间
/// （见 [`mark_manual_view_change`]）。播放头一动它自然对不上、标记失效，跟随随之恢复；
/// 不需要任何额外的清除钩子，也就不会有「忘了清、跟随再也不回来」的状态残留。
///
/// 未放大时 [`clamp_window`] 会把结果按回 `[0, total]`，所以这个函数在 zoom = 1.0 下
/// 不改变任何渲染。
fn follow_playhead(
    win: TimelineWindow,
    cur_time: f64,
    total: f64,
    is_playing: bool,
    manual_anchor: Option<f64>,
) -> TimelineWindow {
    if !cur_time.is_finite() {
        return win;
    }
    let span = win.span();
    if span <= 0.0 {
        return win;
    }
    if !is_playing {
        // 用户手动挪过视图且此后没有导航：窗口保持在用户给的位置（播放头允许离屏）
        if manual_anchor.is_some_and(|t| times_equal(t, cur_time)) {
            return win;
        }
        // 否则只做兜底：播放头还在窗口里就什么都不做，真出去了才把它拉回窗口内
        if cur_time >= win.start && cur_time <= win.end {
            return win;
        }
        let start = if cur_time < win.start {
            cur_time
        } else {
            cur_time - span
        };
        let (start, end) = clamp_window(start, start + span, total);
        return TimelineWindow { start, end };
    }
    let band = TIMELINE_FOLLOW_BAND;
    let start = if cur_time < win.start + span * band {
        cur_time - span * band
    } else if cur_time > win.end - span * band {
        cur_time - span * (1.0 - band)
    } else {
        return win;
    };
    let (start, end) = clamp_window(start, start + span, total);
    TimelineWindow { start, end }
}

/// 鼠标横向位置 → 时间轴窗口内的比例（0 = 窗口起点，1 = 窗口终点）。
///
/// 夹在 `[0, 1]`：光标落在轨道标签或右侧留白上时按对应端点缩放，而不是让窗口因为
/// 一个越界的锚点整体漂走。
fn timeline_anchor_ratio(mouse_x: Pixels, win_w: Pixels) -> f64 {
    let track_w = f32::from(win_w - px(Theme::TRACK_LABEL_W) - px(Theme::TRACK_RIGHT_PAD));
    if track_w <= 10.0 {
        return 0.5;
    }
    (f32::from(mouse_x - px(Theme::TRACK_LABEL_W)) / track_w).clamp(0.0, 1.0) as f64
}

/// 一个滚轮事件的「格数」：行模式本身就是格数；像素模式按 50px 一格折算——不同驱动
/// 一次滚轮报 50~120px，不折算会让缩放/平移速度差一倍以上。
fn scroll_notches(event: &ScrollWheelEvent) -> f64 {
    let raw = match event.delta {
        ScrollDelta::Lines(p) => p.y as f64,
        ScrollDelta::Pixels(p) => f32::from(p.y) as f64 / 50.0,
    };
    raw.clamp(-4.0, 4.0)
}

/// 片段两端拖拽热区的宽度（px）。
///
/// 6px 是「抓得住、又不吃掉可点区域」的折中：再窄鼠标点不中，再宽则最短的片段
/// （[`Theme::SEG_CLIP_MIN_W`] = 40px）只剩中间一小块能点选/跳播。
const TIMELINE_TRIM_HANDLE_W: f32 = 6.0;

/// 鼠标位移小于该值（px）不算拖拽：手抖一两像素不该把时间挪走几十毫秒。
const TIMELINE_TRIM_SLOP_PX: f32 = 3.0;

/// 拖拽改时间时的最短片段时长（秒）。
///
/// 取 0.1s（与管道里的 `MIN_SEGMENT_DUR` 同值）：一帧约 0.04s，比 0.1s 更短的时间码在
/// 字面上就没有可读意义（SRT/VTT 里 0.1s 已是可用粒度），用户根本看不到这么短的字幕；
/// 而载入工程时 `optimize_segments` 还会把这类片段当幻读鬼影直接删掉。
const TIMELINE_TRIM_MIN_SECS: f64 = crate::subtitle::MIN_SEGMENT_DUR;

/// 拖拽编辑的部位：左端改 `start`、右端改 `end`、中段整段平移。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimelineTrimMode {
    Start,
    End,
    Move,
}

/// 一轮拖拽的会话（按下 → 移动 → 松手）。
///
/// 与 [`TIMELINE_PAN_START`] 同一套 thread_local 范式：窗口结构体定义在 `ui/mod.rs`，
/// 本模块不便向外扩字段（同 `REPLACE_PANEL_TOGGLES`）。
#[derive(Clone, Copy)]
struct TimelineTrimDrag {
    /// 被拖拽片段的 `Segment::index`。不用下标：拖拽途中任何一次拆分/合并都会让下标
    /// 指向别的句子，`index` 才是清单与列表共用的稳定标识。
    index: usize,
    mode: TimelineTrimMode,
    /// 按下前的原始时间：判定「有没有真的改动」、以及推撤销快照时写回原值都要它。
    origin_start: f64,
    origin_end: f64,
    /// 按下时的鼠标横坐标：拖动量取「按下点 → 当前点」的时间差，热区抓偏一点也不会跳。
    press_x: Pixels,
}

thread_local! {
    static TIMELINE_TRIM_DRAG: std::cell::Cell<Option<TimelineTrimDrag>> =
        const { std::cell::Cell::new(None) };
    /// 「刚拖过时间」标记：松手时 GPUI 仍会补一次 click（按下/松开都在片段内），那一次
    /// 不该再顺手跳播。按下任何热区/片段时清零（见 `begin_timeline_trim`）。
    static TIMELINE_TRIM_CLICK_GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 记一笔「用户手动调过视图」：存下手动那一刻的播放头时间。
/// 播放头一动（seek / 播放推进）标记自动失效，跟随随之恢复（见 [`follow_playhead`]）。
fn mark_manual_view_change(playhead: f64) {
    TIMELINE_PAN_ANCHOR.with(|a| a.set(Some(playhead)));
}

/// 最小时长的兜底：非有限或非正值一律退回常量，免得 NaN 渗进夹紧区间。
fn sane_min_dur(min_dur: f64) -> f64 {
    if min_dur.is_finite() && min_dur > 0.0 {
        min_dur
    } else {
        TIMELINE_TRIM_MIN_SECS
    }
}

/// 夹紧到 `[lower, upper]`；区间非法或输入非有限时退回 `fallback`。
///
/// 退回原值而不是硬夹到某个端点：数据本身已经坏掉（前一句压着后一句）时，宁可一动
/// 不动，也别顺手把时间码改成另一个坏值。
fn clamp_trim_time(raw: f64, lower: f64, upper: f64, fallback: f64) -> f64 {
    if !raw.is_finite() || !lower.is_finite() || !upper.is_finite() || lower > upper {
        return fallback;
    }
    raw.clamp(lower, upper)
}

/// 拖拽左端：返回夹紧后的新 `start`（纯函数，不带任何 UI 状态）。
///
/// 约束：`start >= 0`；`start >= prev_end`（不许拖到前一句之前，相邻顺序不变）；
/// `start <= end - min_dur`；`start <= total - min_dur`。
fn trim_start(
    raw: f64,
    cur_start: f64,
    cur_end: f64,
    prev_end: Option<f64>,
    total: f64,
    min_dur: f64,
) -> f64 {
    let min_dur = sane_min_dur(min_dur);
    // 上界 = 「本句终点 - 最小时长」：终点不动，起点就不能顶穿它。总时长只是一层额外的
    // 天花板（正常数据必然 end <= total），防的是上游给了越界的 end。
    let upper = if total.is_finite() && total > 0.0 {
        cur_end.min(total) - min_dur
    } else {
        cur_end - min_dur
    };
    let lower = prev_end
        .filter(|v| v.is_finite())
        .map_or(0.0, |v| v.max(0.0));
    clamp_trim_time(raw, lower, upper, cur_start)
}

/// 拖拽右端：返回夹紧后的新 `end`。
///
/// 约束：`end >= start + min_dur`；`end <= next_start`（不许压过后一句的起点）；
/// `end <= total`。
fn trim_end(
    raw: f64,
    cur_start: f64,
    cur_end: f64,
    next_start: Option<f64>,
    total: f64,
    min_dur: f64,
) -> f64 {
    let min_dur = sane_min_dur(min_dur);
    let upper = match (
        total.is_finite() && total > 0.0,
        next_start.filter(|v| v.is_finite()),
    ) {
        (true, Some(next)) => total.min(next),
        (true, None) => total,
        (false, Some(next)) => next,
        // 既没有总时长也没有下一句：没有可用的上界，别猜
        (false, None) => return cur_end,
    };
    clamp_trim_time(raw, cur_start + min_dur, upper, cur_end)
}

/// 整段平移：时长不变，返回夹紧后的 `(start, end)`。
///
/// 越界体现在「能挪多远」：先算出允许的位移区间（左端不得越过前一句终点、也不得为负；
/// 右端不得越过下一句起点与总时长），再把请求位移夹进去——够不着就少挪，绝不变时长。
fn shift_segment(
    delta: f64,
    cur_start: f64,
    cur_end: f64,
    prev_end: Option<f64>,
    next_start: Option<f64>,
    total: f64,
    min_dur: f64,
) -> (f64, f64) {
    let dur = cur_end - cur_start;
    // 退化片段（时长非正、比最小可见时长还短）不参与平移：挪了也还是坏数据
    if !delta.is_finite() || !dur.is_finite() || dur < sane_min_dur(min_dur) {
        return (cur_start, cur_end);
    }
    let lower = prev_end
        .filter(|v| v.is_finite())
        .map_or(0.0, |v| v.max(0.0));
    let upper = match (
        total.is_finite() && total > 0.0,
        next_start.filter(|v| v.is_finite()),
    ) {
        (true, Some(next)) => total.min(next),
        (true, None) => total,
        (false, Some(next)) => next,
        (false, None) => return (cur_start, cur_end),
    };
    let min_delta = lower - cur_start;
    let max_delta = upper - cur_end;
    if min_delta > max_delta {
        // 前后邻居已经把它夹死（没有任何合法位置）：原地不动
        return (cur_start, cur_end);
    }
    let d = delta.clamp(min_delta, max_delta);
    (cur_start + d, cur_end + d)
}

/// 两个时间是否算「没变」。1e-9 秒远低于时间码精度（0.1s），只用来吃掉浮点毛刺：
/// 「拖出去又拖回原处」必须判定成没变，否则会白留一个撤销点。
fn times_equal(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// 鼠标横坐标 → 可视窗口内的时间（轨道太窄时算不出来，返回 `None`）。
///
/// 偏移量与 `MainWindow::seek_by_mouse_x` 同源（`TRACK_LABEL_W` / `TRACK_RIGHT_PAD`），
/// 并且只走 [`TimelineWindow`] 这一条变换——拖拽热区和播放头必须落在同一套坐标里，
/// 各算一套就必然错位。
fn timeline_time_at_x(mouse_x: Pixels, window_width: Pixels, win: TimelineWindow) -> Option<f64> {
    let track_w = window_width - px(Theme::TRACK_LABEL_W) - px(Theme::TRACK_RIGHT_PAD);
    if f32::from(track_w) <= 10.0 {
        return None;
    }
    let offset = f32::from(mouse_x - px(Theme::TRACK_LABEL_W));
    let ratio = (offset / f32::from(track_w)) as f64;
    Some(win.ratio_to_time(ratio))
}

/// 片段左右两端的拖拽热区（窄条，平时透明；选中时各画一条细亮条做提示）。
///
/// 为什么做成独立子元素、而不是「片段中部按下 + 自己判断左右 6px」：GPUI 的鼠标事件按
/// hitbox 冒泡，独立热区才能各自带 `col-resize` 光标，并用 `stop_propagation` 独占这次
/// 按下——中段平移与轨道定位都不许再接手。
fn trim_handles(index: usize, selected: bool, cx: &Context<MainWindow>) -> [AnyElement; 2] {
    let handle = |mode: TimelineTrimMode, on_left: bool| -> AnyElement {
        div()
            .id(if on_left {
                ("timeline-trim-start", index)
            } else {
                ("timeline-trim-end", index)
            })
            .absolute()
            .top_0()
            .bottom_0()
            .w(px(TIMELINE_TRIM_HANDLE_W))
            .when(on_left, |d| d.left_0())
            .when(!on_left, |d| d.right_0())
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::ResizeLeftRight)
            .when(selected, |d| {
                d.child(
                    div()
                        .w(px(2.0))
                        .h(px(Theme::CTRL_H_XS))
                        .rounded_full()
                        .bg(Theme::text_on_saturated()),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.begin_timeline_trim(
                        index,
                        mode,
                        event.position.x,
                        window.viewport_size().width,
                    );
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(
                cx.listener(move |this, event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button == Some(MouseButton::Left)
                        && this.update_timeline_trim(event.position.x, window.viewport_size().width)
                    {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _event: &MouseUpEvent, _window, cx| {
                    // 没拖动过的那次松手仍算「点选 + 跳播」；真拖了就只落这一次编辑
                    if !TIMELINE_TRIM_CLICK_GUARD.with(|g| g.get()) {
                        this.select_timeline_segment(index, cx);
                    }
                    this.commit_timeline_trim(cx);
                    // 热区自己收尾，别让片段再把这次松手当 click（拖完又跳一次）
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    };
    [
        handle(TimelineTrimMode::Start, true),
        handle(TimelineTrimMode::End, false),
    ]
}

impl MainWindow {
    /// 开始一轮拖拽编辑：记下按下点与原始时间，**不**动数据、不记撤销。
    fn begin_timeline_trim(
        &self,
        index: usize,
        mode: TimelineTrimMode,
        press_x: Pixels,
        window_width: Pixels,
    ) {
        // 新一轮按下先清零「刚拖过」标记（上一个会话可能没走到 click 就结束了）
        TIMELINE_TRIM_CLICK_GUARD.with(|g| g.set(false));
        let Some((origin_start, origin_end)) = self
            .state
            .segments
            .iter()
            .find(|s| s.index == index)
            .map(|s| (s.start, s.end))
        else {
            TIMELINE_TRIM_DRAG.with(|d| d.set(None));
            return;
        };
        if timeline_time_at_x(press_x, window_width, self.timeline_window()).is_none() {
            // 轨道太窄，换算不出按下点：宁可不拖，也别产生半截编辑
            TIMELINE_TRIM_DRAG.with(|d| d.set(None));
            return;
        }
        TIMELINE_TRIM_DRAG.with(|d| {
            d.set(Some(TimelineTrimDrag {
                index,
                mode,
                origin_start,
                origin_end,
                press_x,
            }))
        });
    }

    /// 拖拽中：把鼠标位置换算成新时间写进 `segments`（实时预览）。
    ///
    /// 这里**不**记撤销、**不**置脏、**不**递增修订号：一次拖动会来几十个鼠标事件，每个
    /// 都落一次库既没用又卡；正式落账统一在 [`MainWindow::commit_timeline_trim`]。
    /// 返回是否真的改了时间（调用方据此重绘）。
    ///
    /// 拖拽途中改的是 `segments` 本体而修订号不动，派生快照里的时间轴排序索引暂时是旧
    /// 的；被拖的片段本来就在可视范围内（否则点不到），不会中途从轨道上消失，松手递增
    /// 修订号后索引立刻归位。
    fn update_timeline_trim(&mut self, mouse_x: Pixels, window_width: Pixels) -> bool {
        let Some(drag) = TIMELINE_TRIM_DRAG.with(|d| d.get()) else {
            return false;
        };
        // 手抖一两像素不算拖拽：否则每次点选都会顺手把时间挪走几十毫秒
        if (f32::from(mouse_x) - f32::from(drag.press_x)).abs() < TIMELINE_TRIM_SLOP_PX {
            return false;
        }
        let win = self.timeline_window();
        let (Some(press_time), Some(now_time)) = (
            timeline_time_at_x(drag.press_x, window_width, win),
            timeline_time_at_x(mouse_x, window_width, win),
        ) else {
            return false;
        };
        let delta = now_time - press_time;
        let total = self.timeline_total_dur();
        let Some(pos) = self
            .state
            .segments
            .iter()
            .position(|s| s.index == drag.index)
        else {
            return false;
        };
        // 邻居按存储顺序取 `pos ± 1`（与 `adjust_selected_times` 同口径）；拖拽期间不动
        // 邻居，所以每帧现取与按下那一刻等价。
        let prev_end = pos.checked_sub(1).map(|p| self.state.segments[p].end);
        let next_start = self.state.segments.get(pos + 1).map(|s| s.start);
        let (start, end) = match drag.mode {
            TimelineTrimMode::Start => (
                trim_start(
                    drag.origin_start + delta,
                    drag.origin_start,
                    drag.origin_end,
                    prev_end,
                    total,
                    TIMELINE_TRIM_MIN_SECS,
                ),
                drag.origin_end,
            ),
            TimelineTrimMode::End => (
                drag.origin_start,
                trim_end(
                    drag.origin_end + delta,
                    drag.origin_start,
                    drag.origin_end,
                    next_start,
                    total,
                    TIMELINE_TRIM_MIN_SECS,
                ),
            ),
            TimelineTrimMode::Move => shift_segment(
                delta,
                drag.origin_start,
                drag.origin_end,
                prev_end,
                next_start,
                total,
                TIMELINE_TRIM_MIN_SECS,
            ),
        };
        let seg = &mut self.state.segments[pos];
        if times_equal(seg.start, start) && times_equal(seg.end, end) {
            // 拖回原处：时间没变，别点亮「刚拖过」标记（松手时也不该留撤销点）
            return false;
        }
        seg.start = start;
        seg.end = end;
        TIMELINE_TRIM_CLICK_GUARD.with(|g| g.set(true));
        true
    }

    /// 松手落账：一轮拖拽只产生**一次**编辑。
    ///
    /// 三者缺一不可：压一次撤销快照（拖错了要有路可退）、置 `segments_dirty`（不置脏时
    /// 关窗守卫读到的仍是 false，这次改动会被当成「无改动」丢掉）、
    /// `bump_segments_revision`（只失效清单缓存、不自己落库，见 `AppState` 上的同款注释）。
    fn commit_timeline_trim(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = TIMELINE_TRIM_DRAG.with(|d| d.take()) else {
            return;
        };
        let Some(pos) = self
            .state
            .segments
            .iter()
            .position(|s| s.index == drag.index)
        else {
            return;
        };
        let (start, end) = (self.state.segments[pos].start, self.state.segments[pos].end);
        // 时间一点没动（压根没拖 / 拖回原处）：不留撤销点、不置脏——白压一层撤销点会让
        // Ctrl+Z 退无可退，白置脏则会把一份没变的文档写回库。
        if times_equal(start, drag.origin_start) && times_equal(end, drag.origin_end) {
            return;
        }
        // 撤销快照必须记「按下前的原样」：拖拽期间改的就是 `segments` 本体，直接推当前值
        // 等于把改后的状态当成撤销目标，Ctrl+Z 会退无可退。先把原值写回、推栈、再落回新值。
        self.state.segments[pos].start = drag.origin_start;
        self.state.segments[pos].end = drag.origin_end;
        self.state.snapshot_for_undo();
        self.state.segments[pos].start = start;
        self.state.segments[pos].end = end;
        self.state.segments_dirty = true;
        self.state.bump_segments_revision();
        self.state.flush_segments_if_dirty();
        cx.notify();
    }

    /// 点选片段并跳播（片段中部点击、热区上「没拖动」的松手共用同一套动作）。
    fn select_timeline_segment(&mut self, index: usize, cx: &mut Context<Self>) {
        self.state.select_segment(index);
        self.trigger_extract_frame(cx);
        cx.notify();
    }
}

impl MainWindow {
    /// 时间轴按到的总时长：与改造前的 `total_duration.max(1.0)` 同源，
    /// 空白工程下也要留出一条完整的时间轴。
    fn timeline_total_dur(&self) -> f64 {
        self.state.total_duration.max(1.0)
    }

    /// 当前可视窗口（不含播放跟随，见 [`MainWindow::timeline_window_followed`]）。
    ///
    /// 渲染与鼠标定位必须都从这里取值：两处各算各的，点击落点就会和播放头错位。
    fn timeline_window(&self) -> TimelineWindow {
        timeline_window(
            self.state.timeline_zoom,
            TIMELINE_PAN_START.with(|p| p.get()),
            self.timeline_total_dur(),
        )
    }

    /// 渲染用的窗口：在 [`MainWindow::timeline_window`] 之上做播放头跟随，并把跟随后的
    /// 起点写回平移量。只影响下一帧，不需要 `cx.notify()`——本帧本来就在渲染。
    fn timeline_window_followed(&self, cur_time: f64) -> TimelineWindow {
        let win = self.timeline_window();
        let followed = follow_playhead(
            win,
            cur_time,
            self.timeline_total_dur(),
            self.state.is_playing,
            // 暂停时「手动调过视图」的标记：内容是手动那一刻的播放头时间（见 follow_playhead）
            TIMELINE_PAN_ANCHOR.with(|a| a.get()),
        );
        if followed.start != win.start {
            TIMELINE_PAN_START.with(|p| p.set(followed.start));
        }
        followed
    }

    /// 以 `anchor_ratio` 为不动点缩放，并把夹紧后的窗口起点存回平移量。
    /// 以 `anchor_ratio` 为不动点缩放，并把夹紧后的窗口起点存回平移量。
    ///
    /// 缩放也算「用户手动调过视图」：暂停时窗口不该被播放头拽回，否则锚点选在别处时，
    /// 下一帧渲染就被 [`follow_playhead`] 拉走，放大到哪都白给。
    fn zoom_timeline(&mut self, anchor_ratio: f64, factor: f64, cx: &mut Context<Self>) {
        let total = self.timeline_total_dur();
        let start = TIMELINE_PAN_START.with(|p| p.get());
        let (zoom, start) = zoom_at(anchor_ratio, start, total, self.state.timeline_zoom, factor);
        self.state.timeline_zoom = zoom;
        TIMELINE_PAN_START.with(|p| p.set(start));
        mark_manual_view_change(self.state.current_time);
        cx.notify();
    }

    /// 水平平移 `delta_spans`（以窗口宽度为单位，正值 = 往更晚的时间看）。
    ///
    /// 平移是「用户手动调过视图」的典型动作：记下这一刻的播放头时间，暂停时
    /// [`follow_playhead`] 就不再把它拽回来——这里正是「暂停时向右滚完全没反应」的根。
    fn pan_timeline(&mut self, delta_spans: f64, cx: &mut Context<Self>) {
        let total = self.timeline_total_dur();
        let win = self.timeline_window();
        let span = win.span();
        let start = win.start + delta_spans * span;
        let (start, _) = clamp_window(start, start + span, total);
        TIMELINE_PAN_START.with(|p| p.set(start));
        mark_manual_view_change(self.state.current_time);
        cx.notify();
    }

    /// 复位到整段视图。
    fn reset_timeline_zoom(&mut self, cx: &mut Context<Self>) {
        self.state.timeline_zoom = TIMELINE_ZOOM_MIN;
        TIMELINE_PAN_START.with(|p| p.set(0.0));
        cx.notify();
    }
}

impl MainWindow {
    /// 渲染时间轴波形轨 (F-014)。
    ///
    /// 包络在后台由 FFmpeg 解码成 8kHz 单声道后按时间桶取峰值（见
    /// `engines::waveform`），这里只做「按可视窗口重采样到固定柱数 + 上色」两件事。
    ///
    /// 固定柱数而非直接铺满桶数，是为了让每根柱在任意窗口宽度下都对应一段稳定的
    /// 时间切片，同时把元素数量压在两百个量级，播放时逐帧重绘不卡。
    ///
    /// 窗口内取峰值直接用 `WaveformData::peaks_in_window`（它就是为可视窗口写的，
    /// 自带单测）：整段重采样与窗口重采样本是同一件事的两个实现，只留窗口这一条路，
    /// 才不会在缩放途中退回「按整段铺满」的坐标。
    fn render_waveform_lane(
        &mut self,
        visible: TimelineWindow,
        progress_ratio: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 波形柱数量：约等于 1000px 时间轴上的 5px 一根，再密也看不出差别
        const BARS: usize = 200;
        // 满量程柱高（px）。轨道内高 26px，留出上下呼吸空间
        const MAX_BAR_H: f32 = 22.0;

        let peaks: Vec<f32> = match self.state.waveform.as_ref() {
            Some(w) if !w.peaks.is_empty() => w.peaks_in_window(visible.start, visible.end, BARS),
            _ => Vec::new(),
        };
        let extracting = self.state.waveform_busy_for.is_some();
        let has_peaks = !peaks.is_empty();

        div()
            .id("timeline-waveform-lane-row")
            .h(px(Theme::TRACK_ROW_H))
            .w_full()
            .flex()
            .items_center()
            .border_t_1()
            .border_color(Theme::border_subtle())
            // 轨道名，与「字幕轨」标签同宽同缩进，保证两条轨的时间刻度对齐
            .child(
                div()
                    .w(px(Theme::TRACK_LABEL_W))
                    .pl(px(Theme::SPACE_3))
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(Theme::accent_blue())
                    .child("波形轨"),
            )
            .child(
                div()
                    .id("timeline-waveform-lane")
                    .flex_1()
                    .h(px(Theme::CTRL_H_XS))
                    .mr(px(Theme::TRACK_RIGHT_PAD))
                    .rounded(px(Theme::RADIUS_LG))
                    .bg(Theme::bg_panel())
                    .border_1()
                    .border_color(Theme::border())
                    .relative()
                    .overflow_hidden()
                    .cursor_pointer()
                    // 波形轨同样支持点击/拖拽定位，避免用户只能在字幕轨上找播放点
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, true, cx);
                        }
                    }))
                    .child(if has_peaks {
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .px_1()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::WAVE_BAR_GAP))
                            .children(peaks.into_iter().enumerate().map(|(i, peak)| {
                                // 已播过的一半用薄荷色，未播的一半用中性灰，形成进度感
                                let played = (i as f32 + 0.5) / BARS as f32 <= progress_ratio;
                                div().flex_1().flex().justify_center().items_center().child(
                                    div()
                                        .w_full()
                                        // 静音段也留 1px 细线，保持「整轨连续」的观感
                                        .h(px((peak * MAX_BAR_H).max(Theme::WAVE_BAR_MIN_H)))
                                        .rounded_full()
                                        .bg(if played {
                                            Theme::accent_mint()
                                        } else {
                                            Theme::bg_dot_idle()
                                        }),
                                )
                            }))
                            .into_any_element()
                    } else {
                        // 提取中 / 提取失败 / 无音轨：给出可辨识的占位文案，而不是一条空白槽
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child(if extracting {
                                "波形提取中…"
                            } else {
                                "无波形数据"
                            })
                            .into_any_element()
                    })
                    // 播放游标，与字幕轨的指针保持同一条垂直线
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(relative(progress_ratio))
                            .w(px(Theme::PLAYHEAD_W))
                            .bg(Theme::accent_mint()),
                    ),
            )
    }

    /// 渲染专业多轨剪辑时间轴 (Multi-track Timeline - 剪映/Premiere风格)
    pub(crate) fn render_multitrack_timeline(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let total_dur = self.state.total_duration.max(1.0);
        let cur_time = self.state.current_time;
        // 时间 → 像素只认这一个窗口：片段、播放头、波形、刻度、鼠标定位全走它
        let win = self.timeline_window_followed(cur_time);
        let progress_ratio = win.time_to_ratio(cur_time).clamp(0.0, 1.0) as f32;
        let zoom = self.state.timeline_zoom;
        let sel_idx = self.state.selected_segment_index;

        // 拖拽中的时间提示：显示正在编辑的那一端（右端热区 = 终点，其余 = 起点）。
        // 只给时间码，不加说明文字——拖动时眼睛在片段上，多一个字都是噪声。
        let trim_hint: Option<String> = TIMELINE_TRIM_DRAG.with(|d| d.get()).and_then(|drag| {
            self.state
                .segments
                .iter()
                .find(|s| s.index == drag.index)
                .map(|s| {
                    let t = match drag.mode {
                        TimelineTrimMode::End => s.end,
                        TimelineTrimMode::Start | TimelineTrimMode::Move => s.start,
                    };
                    seconds_to_timestamp_short(t)
                })
        });

        div()
            .id("editor-multitrack-timeline")
            .h(px(Theme::TIMELINE_H))
            .flex_shrink_0()
            .w_full()
            .bg(Theme::bg_sidebar())
            .border_t_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .overflow_hidden()
            // 时间轴独占 Ctrl/Shift + 滚轮：缩放与平移不该顺带把外层页面滚了
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let notches = scroll_notches(event);
                if notches == 0.0 {
                    return;
                }
                if event.modifiers.control {
                    // 锚点取鼠标位置：滚轮指着的那一帧缩放前后必须是同一个像素
                    let win_w = window.viewport_size().width;
                    let anchor = timeline_anchor_ratio(event.position.x, win_w);
                    this.zoom_timeline(anchor, TIMELINE_ZOOM_SCROLL_STEP.powf(notches), cx);
                    cx.stop_propagation();
                } else if event.modifiers.shift {
                    // 滚轮向前 = 看更早的时间（同浏览器 Shift+滚轮横向滚动的口径）
                    this.pan_timeline(-notches * TIMELINE_PAN_STEP, cx);
                    cx.stop_propagation();
                }
            }))
            // 拖拽改时间的收尾兜底：松手时指针可能已经不在片段/热区上（拖到别的轨、
            // 甚至拖出时间轴）。漏掉收尾会把「已改但没置脏」留在内存里，改动静默丢失。
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    this.commit_timeline_trim(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    this.commit_timeline_trim(cx);
                }),
            )
            // 时间轴顶栏工具区 (紧凑 28px)
            .child(
                div()
                    .h(px(Theme::CTRL_H_SM))
                    .px(px(Theme::SPACE_3))
                    .bg(Theme::bg_sidebar())
                    .border_b_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_primary())
                                    .child("时间轴"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(Theme::bg_inset())
                                    .border_1()
                                    .border_color(Theme::border_mid())
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_family("Consolas")
                                    .text_color(Theme::accent_mint())
                                    .font_weight(FontWeight::BOLD)
                                    .child(seconds_to_hms(cur_time)),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_muted())
                                    .child(format!(
                                        "共 {} 句 · 总长 {}",
                                        self.state.segments.len(),
                                        format_duration_short(total_dur)
                                    )),
                            )
                            // 拖拽中把时间码顶到工具条上：位置固定、文案只有时间码
                            .children(trim_hint.clone().map(|hint| {
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(Theme::bg_inset())
                                    .border_1()
                                    .border_color(Theme::accent_primary())
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_family("Consolas")
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::accent_primary())
                                    .child(hint)
                            })),
                    )
                    // 右侧：缩放控件 + 快捷键速查（后者与 shortcuts::SHORTCUT_HINTS 同源，
                    // 只挑高频几项，避免占满工具条）
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            // 缩放三键：按钮拿不到鼠标位置，锚点固定为窗口中心
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(Self::render_mini_btn(
                                        "timeline-zoom-out",
                                        "-",
                                        zoom > TIMELINE_ZOOM_MIN,
                                        cx,
                                        |this, cx| {
                                            this.zoom_timeline(
                                                0.5,
                                                1.0 / TIMELINE_ZOOM_BTN_STEP,
                                                cx,
                                            )
                                        },
                                    ))
                                    .child(Self::render_mini_btn(
                                        "timeline-zoom-reset",
                                        "复位",
                                        zoom > TIMELINE_ZOOM_MIN,
                                        cx,
                                        |this, cx| this.reset_timeline_zoom(cx),
                                    ))
                                    .child(Self::render_mini_btn(
                                        "timeline-zoom-in",
                                        "+",
                                        zoom < TIMELINE_ZOOM_MAX,
                                        cx,
                                        |this, cx| {
                                            this.zoom_timeline(0.5, TIMELINE_ZOOM_BTN_STEP, cx)
                                        },
                                    )),
                            )
                            .children(
                                crate::ui::shortcuts::SHORTCUT_HINTS
                                    .iter()
                                    .filter(|(key, _)| {
                                        // 撤销是这次新加的高频操作，不进这个白名单的话
                                        // 提示只写在常量里、用户永远看不到
                                        matches!(
                                            *key,
                                            "Ctrl+Space"
                                                | "Alt+↑↓"
                                                | "Ctrl+F"
                                                | "Ctrl+E"
                                                | "Ctrl+Z"
                                        )
                                    })
                                    .map(|(key, label)| {
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .px_1p5()
                                                    .py_0p5()
                                                    .rounded(px(Theme::RADIUS_SM))
                                                    .bg(Theme::bg_inset())
                                                    .border_1()
                                                    .border_color(Theme::border_mid())
                                                    .text_size(px(Theme::TEXT_CAPTION))
                                                    .font_family("Consolas")
                                                    .text_color(Theme::text_secondary())
                                                    .child(*key),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(Theme::TEXT_CAPTION))
                                                    .text_color(Theme::text_muted())
                                                    .child(*label),
                                            )
                                    }),
                            ),
                    ),
            )
            // 1. 统一主字幕轨道 (紧凑苗条单轨布局，高 34px)
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .relative()
                    .flex()
                    .items_center()
                    .py_1()
                    .child(
                        div()
                            .h_full()
                            .w_full()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .w(px(Theme::TRACK_LABEL_W))
                                    .pl(px(Theme::SPACE_3))
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::accent_mint())
                                    .child("字幕轨"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h(px(Theme::CTRL_H_MD))
                                    .mr(px(Theme::TRACK_RIGHT_PAD))
                                    .rounded_lg()
                                    .bg(Theme::bg_panel())
                                    .border_1()
                                    .border_color(Theme::border())
                                    .relative()
                                    .overflow_hidden()
                                    // 进度指示底色
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(progress_ratio))
                                            .bg(Theme::tint_mint_soft()),
                                    )
                                    // 字幕片段渲染 (紧凑纤细卡片，高度 28px)
                                    .children({
                                        // 窗口内的片段走派生快照里的排序索引二分（见
                                        // `timeline_clip_positions`）：不再每帧全量扫一遍
                                        // 片段表——几千~几万句时那是每帧一次 O(n)。
                                        let snapshot = self.derived_snapshot();
                                        let segments: &[Segment] = &self.state.segments;
                                        let positions = timeline_clip_positions(
                                            segments,
                                            &snapshot.timeline,
                                            cur_time,
                                            sel_idx,
                                        );
                                        // 元素在块内就建好：下面的 `cx.listener` 要独占
                                        // 借用 `cx`，闭包再借走块内的局部切片会活不过这里
                                        // （`Vec<AnyElement>` 与原来的惰性迭代等价，画出来的
                                        // 元素、顺序、样式完全一致）。
                                        let clips: Vec<AnyElement> = positions
                                            .into_iter()
                                            .map(|pos| {
                                                let seg = &segments[pos];
                                                let seg_idx = seg.index;
                                                // 左偏移不夹紧：窗口外的片段要能拿到负值 /
                                                // 大于 1 的值，靠容器裁掉，而不是全挤在两端
                                                let start_r = win.time_to_ratio(seg.start) as f32;
                                                // 宽度按窗口折算，放大后短句不会缩成一条线
                                                let width_r = (((seg.end - seg.start) / win.span())
                                                    .clamp(0.015, 1.0)
                                                    as f32)
                                                    .max(0.02);
                                                let is_selected = sel_idx == Some(seg_idx);
                                                // 与列表高亮、AppState::get_active_segment() 共用半开
                                                // 区间 [start, end)：共享边界属于下一句（见 is_playing_at）。
                                                let is_active =
                                                    is_playing_at(seg.start, seg.end, cur_time);
                                                div()
                                                    .id(("timeline-clip", seg_idx))
                                                    .absolute()
                                                    .top(px(Theme::TRACK_CLIP_INSET))
                                                    .bottom(px(Theme::TRACK_CLIP_INSET))
                                                    .left(relative(start_r))
                                                    .w(relative(width_r))
                                                    .min_w(px(Theme::SEG_CLIP_MIN_W))
                                                    .rounded(px(Theme::RADIUS_SM))
                                                    .bg(if is_selected {
                                                        Theme::accent_primary()
                                                    } else if is_active {
                                                        Theme::accent_mint_deep()
                                                    } else {
                                                        Theme::bg_dot_idle()
                                                    })
                                                    .border_1()
                                                    .border_color(if is_selected {
                                                        Theme::text_on_saturated()
                                                    } else if is_active {
                                                        Theme::accent_mint()
                                                    } else {
                                                        Theme::tint_neutral_border()
                                                    })
                                                    .cursor_pointer()
                                                    .px_1p5()
                                                    .flex()
                                                    .items_center()
                                                    .overflow_hidden()
                                                    .hover(|s| s.opacity(0.85))
                                                    // 中段按下 = 整段平移（按下只记会话，真移动了才算拖拽）
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |this, event: &MouseDownEvent, window, _cx| {
                                                            this.begin_timeline_trim(
                                                                seg_idx,
                                                                TimelineTrimMode::Move,
                                                                event.position.x,
                                                                window.viewport_size().width,
                                                            );
                                                        }),
                                                    )
                                                    .on_mouse_move(cx.listener(
                                                        move |this, event: &MouseMoveEvent, window, cx| {
                                                            if event.pressed_button == Some(MouseButton::Left)
                                                                && this.update_timeline_trim(
                                                                    event.position.x,
                                                                    window.viewport_size().width,
                                                                )
                                                            {
                                                                cx.notify();
                                                            }
                                                        },
                                                    ))
                                                    .on_mouse_up(
                                                        MouseButton::Left,
                                                        cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                                                            this.commit_timeline_trim(cx);
                                                        }),
                                                    )
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        // 刚拖过时间的那次松手，GPUI 仍会补一次 click：
                                                        // 这一下不该再跳播，否则改完时间画面又跳回去
                                                        if TIMELINE_TRIM_CLICK_GUARD.with(|g| g.get()) {
                                                            return;
                                                        }
                                                        this.select_timeline_segment(seg_idx, cx);
                                                    }))
                                                    .children(trim_handles(seg_idx, is_selected, cx))
                                                    .child(
                                                        div()
                                                            .text_size(px(Theme::TEXT_SMALL))
                                                            // 未选中的片段底色是**翻转**的中性槽，文字须跟着翻；
                                                            // 选中/激活态是深色饱和块，改用恒白文字
                                                            .text_color(
                                                                if is_selected || is_active {
                                                                    Theme::text_on_saturated()
                                                                } else {
                                                                    Theme::text_primary()
                                                                },
                                                            )
                                                            .child(seg.display_text().to_string()),
                                                    )
                                                    .into_any_element()
                                            })
                                            .collect();
                                        clips
                                    }),
                            ),
                    )
                    // 贯穿轨道的交互响应层与播放游标指针
                    .child(
                        div()
                            .id("timeline-playhead-interactive-surface")
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(Theme::TRACK_LABEL_W))
                            .right(px(Theme::TRACK_RIGHT_PAD))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    let win_w = window.viewport_size().width;
                                    this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                |this, event: &MouseMoveEvent, window, cx| {
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        let win_w = window.viewport_size().width;
                                        // 拖拽改时间时由这一层把拖动跟到底：片段被邻居/边界夹住
                                        // 之后指针会跑到片段外，片段自己的 move 事件收不到
                                        if this.update_timeline_trim(event.position.x, win_w) {
                                            cx.notify();
                                        }
                                        // 拖拽期间这条定位会被 seek_by_mouse_x 自己拦下
                                        this.seek_by_mouse_x(event.position.x, win_w, true, cx);
                                    }
                                },
                            ))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                                    let win_w = window.viewport_size().width;
                                    this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                                }),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .left(relative(progress_ratio))
                                    .ml(px(-Theme::PLAYHEAD_GRAB_W / 2.0))
                                    .w(px(Theme::PLAYHEAD_GRAB_W))
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .text_color(Theme::accent_mint())
                                            .child("▼"),
                                    )
                                    .child(
                                        div()
                                            .w(px(Theme::PLAYHEAD_W))
                                            .flex_1()
                                            .bg(Theme::accent_mint()),
                                    ),
                            ),
                    ),
            )
            // 2. 音频波形轨 (F-014)：与字幕轨共用同一条时间刻度，方便对着语音峰值卡点
            .child(self.render_waveform_lane(win, progress_ratio, cx))
            // 3. 探底时间刻度标尺 (Time Ruler 移至底部展示，精简 20px)
            .child(
                div()
                    .id("timeline-time-ruler")
                    .h(px(Theme::BADGE_H))
                    .w_full()
                    .bg(Theme::bg_sidebar())
                    .border_t_1()
                    .border_color(Theme::border())
                    .relative()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, true, cx);
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, window, cx| {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(Theme::TRACK_LABEL_W))
                            .right(px(Theme::TRACK_RIGHT_PAD))
                            .children((0usize..=10).map(|i| {
                                // 刻度位置与刻度值都按窗口取：放大后刻度自动变密，
                                // 每一格仍是窗口内的 1/10
                                let ratio = i as f64 / 10.0;
                                let t = win.ratio_to_time(ratio);
                                div()
                                    .id(("ruler-tick-btn", i))
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .left(relative(ratio as f32))
                                    .ml(px(-Theme::RULER_TICK_HIT_W / 2.0))
                                    .w(px(Theme::RULER_TICK_HIT_W))
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.state.seek_to(t);
                                        this.trigger_extract_frame(cx);
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .w(px(Theme::RULER_TICK_W))
                                            .h(px(Theme::RULER_TICK_H))
                                            .bg(Theme::text_muted()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .font_family("Consolas")
                                            .text_color(Theme::text_muted())
                                            .child(format_duration_short(t)),
                                    )
                            })),
                    ),
            )
    }

    /// 统一的时间轴鼠标点击与拖拽精确定位逻辑
    pub(crate) fn seek_by_mouse_x(
        &mut self,
        mouse_x: Pixels,
        window_width: Pixels,
        is_drag: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.total_duration <= 0.0 {
            return;
        }
        // 拖拽改时间期间不再按鼠标位置挪播放头：轨道响应层比片段/热区后画（冒泡里必然
        // 先吃到事件，`stop_propagation` 拦不住它），不在这里拦的话，修右端时指针一路往右、
        // 播放头跟着跑，画面被拽得到处都是。
        if TIMELINE_TRIM_DRAG.with(|d| d.get()).is_some() {
            return;
        }
        self.halt_preview_playback();
        // 两个偏移量必须与布局里的实际值同源，否则点击位置与播放头会整体错位。
        //
        // 注意：导航已从左侧 180px 竖栏改为顶部标签条，时间轴现在从窗口左缘
        // （x=0）起铺满整宽，不再有 `NAV_W` 的左偏移——多减它会让点击位置整体左移 180px。
        //
        // 「鼠标横坐标 → 时间」与拖拽热区共用 [`timeline_time_at_x`]：两处各算各的，
        // 落点就会和播放头差几帧。
        let Some(target_time) = timeline_time_at_x(mouse_x, window_width, self.timeline_window())
        else {
            return;
        };
        self.state.seek_to(target_time);

        if is_drag {
            // 拖拽过程中节流：每 40ms（25帧极速高刷）发起一次单帧抽取请求，极致跟手零延迟
            if self.last_drag_extract.elapsed().as_millis() >= 40 {
                self.last_drag_extract = std::time::Instant::now();
                self.trigger_extract_frame(cx);
            }
        } else {
            // 单击或拖动松手：立刻发起抽取
            self.last_drag_extract = std::time::Instant::now();
            self.trigger_extract_frame(cx);
        }
        cx.notify();
    }

    /// 把工作区选中项拨回「弹窗编辑打开时的那一句」，成功返回 `true`。
    ///
    /// 系统 InputBox **不是本窗口的模态框**：弹窗开着的时候用户仍能去视频库换工程、
    /// 删掉当前记录（`load_task_with` / `clear_current_workspace` 会整体换掉 `segments`），
    /// 连播时播放头推进也会自动改选中项。收尾时若闷头写「当前选中项」，用户敲进对话框的
    /// 文字就会落到别的句子上、甚至别的工程里，并随即落库——属于静默改错文档。
    ///
    /// 因此这里按「弹窗打开时的那份工程 + 那一句下标」定位：工程换了、或那句已不存在，
    /// 返回 `false` 由调用方丢弃结果；同一工程内则把选中项拨回原句，与用户当初点的是同一句。
    fn reanchor_prompt_target(
        &mut self,
        doc: &Option<std::path::PathBuf>,
        target: Option<usize>,
    ) -> bool {
        let Some(target) = target else {
            return false;
        };
        if self.state.selected_file.as_ref() != doc.as_ref() {
            return false;
        }
        if !self.state.segments.iter().any(|s| s.index == target) {
            return false;
        }
        // 走 `select_segment` 让选中项 / 编辑缓冲 / 播放头三者同步，避免后续写回时错位
        self.state.select_segment(target);
        true
    }

    /// 弹出原生 Windows 输入对话框进行字幕文本修改（完美支持搜狗/微软等中文输入法）
    pub(crate) fn prompt_edit_text(&mut self, cx: &mut Context<Self>) {
        // 记下这次弹窗属于哪份文档的哪一句，收尾时据它定位（见 `reanchor_prompt_target`）
        let doc = self.state.selected_file.clone();
        let target = self.state.selected_segment_index;
        let current_text = self.state.editing_text.clone();
        let prompt_title = "Voice2Word - 修改字幕文本";
        let prompt_msg = "请输入修改后的字幕内容（支持中文输入法/粘贴）：";

        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move {
                use std::process::Command;
                let safe_msg = prompt_msg.replace('\'', "''");
                let safe_title = prompt_title.replace('\'', "''");
                let safe_default = current_text.replace('\'', "''");

                let script = format!(
                    "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.Interaction]::InputBox('{}', '{}', '{}')",
                    safe_msg, safe_title, safe_default
                );

                let output = Command::new("powershell")
                    .arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-Command")
                    .arg(&script)
                    .output();

                match output {
                    Ok(out) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !text.is_empty() {
                            Some(text)
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }).await;

            if let Some(new_text) = res {
                let _ = this.update(cx, |this, cx| {
                    // 定位回弹窗打开时的那一句；该句已不属于当前工程则丢弃这次结果
                    if !this.reanchor_prompt_target(&doc, target) {
                        return;
                    }
                    this.state.editing_text = new_text;
                    this.state.save_selected_text();
                    cx.notify();
                });
            }
        }).detach();
    }

    /// 开始就地原地编辑字幕（原文或译文）。
    pub(crate) fn start_inline_edit(
        &mut self,
        seg_idx: usize,
        is_trans: bool,
        initial_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.inline_edit_target.is_some() {
            self.commit_inline_edit(cx);
        }
        self.inline_edit_target = Some((seg_idx, is_trans));
        self.inline_edit_buffer = initial_text.to_string();
        self.inline_edit_cursor = initial_text.chars().count();
        window.focus(&self.inline_edit_focus);
        cx.notify();
    }

    /// 提交保存当前的就地原地编辑。
    pub(crate) fn commit_inline_edit(&mut self, cx: &mut Context<Self>) {
        let Some((seg_idx, is_trans)) = self.inline_edit_target.take() else {
            return;
        };
        let new_text = self.inline_edit_buffer.trim().to_string();
        if is_trans {
            if let Some(seg) = self.state.segments.iter_mut().find(|s| s.index == seg_idx) {
                if new_text.is_empty() {
                    seg.translation = None;
                    seg.translation_lang = None;
                } else {
                    seg.translation = Some(new_text);
                    seg.translation_lang = Some(self.state.translate_target_lang.clone());
                }
            }
        } else {
            if !new_text.is_empty() {
                if let Some(seg) = self.state.segments.iter_mut().find(|s| s.index == seg_idx) {
                    seg.text = new_text.clone();
                    if !seg.polished.is_empty() {
                        seg.polished = new_text.clone();
                    }
                }
                if self.state.selected_segment_index == Some(seg_idx) {
                    self.state.editing_text = new_text;
                }
            }
        }
        self.inline_edit_buffer.clear();
        self.inline_edit_cursor = 0;
        // 就地编辑改的是 `segments` 的正文/译文，必须递增修订号：剪辑台的字幕清单
        // 搜索、术语表高亮与本文件的派生统计快照都按 `segments_revision` 失效。
        // 这里此前只置脏落库、不递增，于是「就地改一句原文」之后，按 revision 缓存
        // 的那几处会一直停在旧值（缓存范式的前提就是「任何改 `segments` 的入口都
        // 递增它」，见 `AppState::bump_segments_revision` 的文档）。
        self.state.bump_segments_revision();
        self.state.segments_dirty = true;
        // 连续改错别字时这是「一句一次写库」（整份 JSON + sqlite UPDATE），
        // 登记成「稍后落库」后由定时器合并成一次（见 SEGMENTS_FLUSH_IDLE）。
        self.state.defer_segments_flush();
        cx.notify();
    }

    /// 取消当前的就地原地编辑。
    pub(crate) fn cancel_inline_edit(&mut self, cx: &mut Context<Self>) {
        self.inline_edit_target = None;
        self.inline_edit_buffer.clear();
        self.inline_edit_cursor = 0;
        cx.notify();
    }

    /// 处理就地原地编辑时的按键输入
    pub(crate) fn handle_inline_edit_keydown(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke.key;
        let total_chars = self.inline_edit_buffer.chars().count();
        let cursor = self.inline_edit_cursor.min(total_chars);

        if event.keystroke.modifiers.control {
            if key == "v" {
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(text) = item.text() {
                        let clean = text.trim().replace(['\r', '\n'], "");
                        let mut chars: Vec<char> = self.inline_edit_buffer.chars().collect();
                        let insert_chars: Vec<char> = clean.chars().collect();
                        let ins_len = insert_chars.len();
                        chars.splice(cursor..cursor, insert_chars);
                        self.inline_edit_buffer = chars.into_iter().collect();
                        self.inline_edit_cursor = cursor + ins_len;
                        cx.notify();
                    }
                }
            } else if key == "c" {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                    self.inline_edit_buffer.clone(),
                ));
            } else if key == "a" {
                self.inline_edit_cursor = total_chars;
                cx.notify();
            }
            return;
        }

        if key == "enter" {
            self.commit_inline_edit(cx);
        } else if key == "escape" {
            self.cancel_inline_edit(cx);
        } else if key == "backspace" {
            if cursor > 0 && total_chars > 0 {
                let mut chars: Vec<char> = self.inline_edit_buffer.chars().collect();
                chars.remove(cursor - 1);
                self.inline_edit_buffer = chars.into_iter().collect();
                self.inline_edit_cursor = cursor - 1;
                cx.notify();
            }
        } else if key == "delete" {
            if cursor < total_chars {
                let mut chars: Vec<char> = self.inline_edit_buffer.chars().collect();
                chars.remove(cursor);
                self.inline_edit_buffer = chars.into_iter().collect();
                cx.notify();
            }
        } else if key == "left" {
            if cursor > 0 {
                self.inline_edit_cursor = cursor - 1;
                cx.notify();
            }
        } else if key == "right" {
            if cursor < total_chars {
                self.inline_edit_cursor = cursor + 1;
                cx.notify();
            }
        } else if key == "home" {
            self.inline_edit_cursor = 0;
            cx.notify();
        } else if key == "end" {
            self.inline_edit_cursor = total_chars;
            cx.notify();
        } else if key.chars().count() == 1 {
            let ch = key.chars().next().unwrap();
            if !ch.is_control() {
                let mut chars: Vec<char> = self.inline_edit_buffer.chars().collect();
                chars.insert(cursor, ch);
                self.inline_edit_buffer = chars.into_iter().collect();
                self.inline_edit_cursor = cursor + 1;
                cx.notify();
            }
        }
    }

    /// 弹出原生 Windows 输入对话框直接订正**译文**（旧版备用入口，已被就地编辑替代）。
    #[allow(dead_code)]
    pub(crate) fn prompt_edit_translation(&mut self, cx: &mut Context<Self>) {
        // 与 `prompt_edit_text` 同样的会话守卫：InputBox 弹出期间工作区可能被换掉、
        // 选中项也可能被播放联动改掉，收尾时必须定位回原来那一句
        // （见 `reanchor_prompt_target`）。
        let doc = self.state.selected_file.clone();
        let target = self.state.selected_segment_index;
        // 预填「当前选中片段的译文」；没有译文则从原文起步，方便用户直接在机翻基础上改。
        let current = self
            .state
            .segments
            .iter()
            .find(|s| Some(s.index) == target)
            .and_then(|s| s.translation.clone())
            .unwrap_or_default();
        let prompt_title = "Voice2Word - 修改译文";
        // 系统 InputBox 在「取消」与「确定但内容为空」两种情况下都返回空串，无法区分。
        // 因此这里把空串当作**空操作**（与 `prompt_edit_text` 一致），避免用户点「取消」
        // 却把已译好的句子清空。要删除译文请用旁边的「清除译文」按钮。
        let prompt_msg = "请输入修改后的译文：（点「取消」不会改动原文；清空请用「清除译文」）";

        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move {
                use std::process::Command;
                let safe_msg = prompt_msg.replace('\'', "''");
                let safe_title = prompt_title.replace('\'', "''");
                let safe_default = current.replace('\'', "''");
                let script = format!(
                    "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.Interaction]::InputBox('{}', '{}', '{}')",
                    safe_msg, safe_title, safe_default
                );
                let output = Command::new("powershell")
                    .arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-Command")
                    .arg(&script)
                    .output();
                match output {
                    Ok(out) if out.status.success() => {
                        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                    }
                    _ => None,
                }
            }).await;

            if let Some(new_text) = res {
                let _ = this.update(cx, |this, cx| {
                    // 定位回弹窗打开时的那一句；该句已不属于当前工程则丢弃这次结果
                    if !this.reanchor_prompt_target(&doc, target) {
                        return;
                    }
                    if !new_text.trim().is_empty() {
                        this.state.set_selected_translation(&new_text);
                        cx.notify();
                    }
                });
            }
        }).detach();
    }

    /// 渲染色彩圆盘调色板（24色相环绕成圈，鼠标滑动调色）
    pub(crate) fn render_color_wheel_picker(
        &self,
        current_hex: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (r, g, b) = crate::utils::SubtitleStyleConfig::hex_to_rgb(current_hex);
        let cur_rgb = rgb((r as u32) << 16 | (g as u32) << 8 | (b as u32));
        let hex_display = current_hex.to_uppercase();

        div()
            .id("custom-color-wheel-panel")
            .w_full()
            .p_2()
            // 拖动取色只改内存；松手（含指针移出面板才松手）在这里统一落盘一次。
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
            .rounded_md()
            .bg(Theme::bg_inset())
            .border_1()
            .border_color(Theme::border())
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            // 左侧：24色相圆环色轮，鼠标滑过即可顺滑调色
            .child(
                div()
                    .w(px(76.0))
                    .h(px(76.0))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    // 24 个色块环绕成一个圈
                    .children((0..24usize).map(|i| {
                        let angle_deg = i as f32 * (360.0 / 24.0);
                        let (pr, pg, pb) = hsv_to_rgb(angle_deg, 1.0, self.color_picker_brightness);
                        let hex_str = format!("#{:02X}{:02X}{:02X}", pr, pg, pb);
                        let dot_color = rgb((pr as u32) << 16 | (pg as u32) << 8 | (pb as u32));
                        let rad = angle_deg.to_radians();
                        let cx_pos = 38.0 + 26.0 * rad.cos() - 4.5;
                        let cy_pos = 38.0 + 26.0 * rad.sin() - 4.5;

                        div()
                            .id(("wheel-dot", i))
                            .absolute()
                            .left(px(cx_pos))
                            .top(px(cy_pos))
                            .w(px(9.0))
                            .h(px(9.0))
                            .rounded_full()
                            .cursor_pointer()
                            .bg(dot_color)
                            .hover(|s| s.border_1().border_color(Theme::text_primary()))
                            .on_mouse_move(cx.listener({
                                let h = hex_str.clone();
                                move |this, event: &MouseMoveEvent, _, cx| {
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        this.state.config.subtitle_style.primary_color = h.clone();
                                        // 打开统一落盘闸门：拖动中只更新内存并同步管线
                                        // （实时预览），真正的写盘留到松手 `end_config_drag`。
                                        this.state.begin_config_drag();
                                        this.state.save_subtitle_style();
                                        cx.notify();
                                    }
                                }
                            }))
                            .on_click(cx.listener({
                                let h = hex_str.clone();
                                move |this, _, _, cx| {
                                    this.state.config.subtitle_style.primary_color = h.clone();
                                    this.state.save_subtitle_style();
                                    cx.notify();
                                }
                            }))
                    }))
                    // 圆环正中心：当前拾取的大色块
                    .child(
                        div()
                            .w(px(26.0))
                            .h(px(26.0))
                            .rounded_full()
                            .bg(cur_rgb)
                            .border_2()
                            .border_color(Theme::border()),
                    ),
            )
            // 右侧：当前十六进制数值与快速纯度调节
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .font_family("Consolas")
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::accent_mint())
                                    .child(hex_display),
                            )
                            .child(
                                div()
                                    .id("wheel-close-btn")
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(Theme::bg_hover())
                                    .cursor_pointer()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.text_color(Theme::text_primary()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.custom_color_picker_open = false;
                                        cx.notify();
                                    }))
                                    .child("收起 ✕"),
                            ),
                    )
                    // 明暗度 / 纯度微调
                    .child(
                        div().flex().items_center().gap_1().children(
                            [
                                ("纯黑", "#000000"),
                                ("深暗", "#1E293B"),
                                ("明亮", "#F8FAFC"),
                                ("金色", "#F59E0B"),
                                ("青翠", "#10B981"),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(idx, (name, hex))| {
                                div()
                                    .id(("preset-light", idx))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .py_0p5()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(Theme::bg_hover())
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| {
                                        s.text_color(Theme::text_primary())
                                            .bg(Theme::bg_hover_strong())
                                    })
                                    .on_click(cx.listener({
                                        let h = hex.to_string();
                                        move |this, _, _, cx| {
                                            this.state.config.subtitle_style.primary_color =
                                                h.clone();
                                            this.state.save_subtitle_style();
                                            cx.notify();
                                        }
                                    }))
                                    .child(name)
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 渲染 Word 风格的数值步进微调输入组件（带加减按钮，中间数值框支持单击/双击就地编辑数字）
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_style_stepper_input<FDown, FUp>(
        &self,
        field: StyleField,
        step_down_label: &str,
        step_up_label: &str,
        step_down_fn: FDown,
        step_up_fn: FUp,
        display_val: String,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        FDown: Fn(&mut MainWindow, &mut Context<MainWindow>) + 'static + Clone,
        FUp: Fn(&mut MainWindow, &mut Context<MainWindow>) + 'static + Clone,
    {
        let is_editing = self.focused_style_field == Some(field);
        let id_prefix = match field {
            StyleField::FontSize => "font-size",
            StyleField::LetterSpacing => "letter-sp",
            StyleField::BottomMargin => "bot-mg",
            StyleField::LineSpacing => "line-sp",
            StyleField::MaxChars => "max-chars",
        };

        let dec_id = format!("{}-dec", id_prefix);
        let inc_id = format!("{}-inc", id_prefix);
        let val_id = format!("{}-val-box", id_prefix);

        let down_fn = step_down_fn.clone();
        let up_fn = step_up_fn.clone();

        let focus_handle = self.style_field_focus.clone();

        div()
            .flex()
            .items_center()
            .gap_1()
            .bg(Theme::bg_inset())
            .border_1()
            // 未编辑时用真正的描边 token：以前这里错用了一个**背景色** token，
            // 在浅色主题下会变成「亮一档的底」压在同一个底上，看起来像没有边框。
            .border_color(if is_editing {
                Theme::accent_mint()
            } else {
                Theme::border_mid()
            })
            .rounded_md()
            .p_0p5()
            // 减号步进按钮
            .child(
                div()
                    .id(SharedString::from(dec_id))
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .rounded_sm()
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_secondary())
                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::accent_mint()))
                    .child(step_down_label.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.focused_style_field.is_some() {
                            this.commit_style_field_edit(cx);
                        }
                        down_fn(this, cx);
                    })),
            )
            // 中间输入框 / 显示框（支持单击与双击直接输入数字）
            .child(if is_editing {
                let cursor = self
                    .line_edit_cursor
                    .min(self.style_field_input.chars().count());
                let before: String = self.style_field_input.chars().take(cursor).collect();
                let after: String = self.style_field_input.chars().skip(cursor).collect();

                div()
                    .id(SharedString::from(val_id))
                    .track_focus(&focus_handle)
                    .w(px(76.0))
                    .px_1p5()
                    .py_0p5()
                    .bg(Theme::bg_app())
                    .border_1()
                    .border_color(Theme::accent_mint())
                    .rounded_sm()
                    .cursor_text()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        let key = &event.keystroke.key;
                        if key == "enter" {
                            this.commit_style_field_edit(cx);
                        } else if key == "escape" {
                            this.cancel_style_field_edit(cx);
                        } else if apply_line_edit(
                            &mut this.style_field_input,
                            &mut this.line_edit_cursor,
                            event,
                            cx,
                        ) {
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(before)
                            .child(div().text_color(Theme::accent_mint()).child("▌"))
                            .child(after),
                    )
            } else {
                div()
                    .id(SharedString::from(val_id))
                    .w(px(76.0))
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .cursor_text()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::accent_mint())
                    .hover(|s| {
                        s.bg(Theme::bg_hover())
                            .border_1()
                            .border_color(Theme::accent_mint())
                    })
                    .child(display_val)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_style_field_edit(field, window, cx);
                    }))
            })
            // 加号步进按钮
            .child(
                div()
                    .id(SharedString::from(inc_id))
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .rounded_sm()
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_secondary())
                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::accent_mint()))
                    .child(step_up_label.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.focused_style_field.is_some() {
                            this.commit_style_field_edit(cx);
                        }
                        up_fn(this, cx);
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_zoom, click_seek_target, fit_video_size, follow_playhead, is_playing_at,
        karaoke_mark_style, karaoke_word_marks, min_gap_hint, monitor_picture_source,
        monitor_screen_size, raw_line_byte_start, render_image_from_frame_file, shift_segment,
        timeline_clip_positions, timeline_time_at_x, timeline_window, trim_end, trim_start,
        zoom_at, MonitorPictureSource, TimelineWindow, WordMark, EDITOR_STACK_MONITOR_H,
        MONITOR_VIDEO_FIT_FRAC, TIMELINE_FULL_DRAW_LIMIT, TIMELINE_TRIM_MIN_SECS,
        TIMELINE_WINDOW_MARGIN_SECS, TIMELINE_ZOOM_MAX, TIMELINE_ZOOM_MIN,
    };
    use crate::subtitle::segment::WordTiming;
    use crate::subtitle::{ExportMode, GapPlan, Segment};
    use crate::ui::actions::translated_out_count;
    use crate::ui::build_timeline_index;
    use crate::ui::primitives::should_consume_scroll;
    use crate::ui::theme::Theme;
    use gpui::FontWeight;

    /// 回归（P2）：「已全部翻译」的判定必须用**实际译出**口径。
    /// 引擎可能给空串/纯空白译文打上 `translation_lang`（或直接复制原文），
    /// 只按语言标签计数会在整片空白时仍显示「已全部翻译」。
    #[test]
    fn only_non_empty_translations_count_as_done() {
        let mut blank = Segment::new(1, 0.0, 1.0, "你好");
        blank.translation = Some("   ".to_string());
        blank.translation_lang = Some("English".to_string()); // 脏标签：有标签无译文
        let mut ok = Segment::new(2, 1.0, 2.0, "世界");
        ok.translation = Some("World".to_string());
        ok.translation_lang = Some("English".to_string());
        let mut other = Segment::new(3, 2.0, 3.0, "再见");
        other.translation = Some("Au revoir".to_string());
        other.translation_lang = Some("Français".to_string());

        let segs = vec![blank, ok, other];
        // 只有 ok 是「当前目标语言 + 非空译文」；blank 的脏标签绝不能算完成。
        assert_eq!(translated_out_count(&segs, "English"), 1);
        assert_eq!(translated_out_count(&segs, "Français"), 1);
        assert_eq!(translated_out_count(&segs, "日本語"), 0);
    }

    /// 点击字幕清单某行的跳播目标时间：取该句 start。
    /// 该行已被删除 / 索引越界时返回 None（调用方据此跳过 seek），不得 panic。
    #[test]
    fn click_seek_target_maps_row_to_segment_start() {
        let segs = vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 3.5, 5.0, "第二句"),
        ];
        assert_eq!(click_seek_target(&segs, 1), Some(0.0));
        assert_eq!(click_seek_target(&segs, 2), Some(3.5));
        // 越界 / 已删除的行：返回 None 而不是 panic
        assert_eq!(click_seek_target(&segs, 99), None);
        assert_eq!(click_seek_target(&[], 1), None);
    }

    /// 内嵌清单滚轮判据：有余量就吃掉（阻止冒泡），到边界才放行给外层。
    ///
    /// 回归现象：在字幕清单里滚轮，整个右侧面板也跟着滚。根因是 GPUI 的滚动
    /// 监听不 `stop_propagation`，内嵌清单与外层可滚动面板同时吃同一个事件。
    #[test]
    fn nested_list_consumes_scroll_until_it_hits_the_edge() {
        // 可滚动区间 100px：offset 从 0（顶部）到 -100（底部）
        let max_h = 100.0;

        // 在中段：两个方向都有余量，清单自己吃掉
        assert!(should_consume_scroll(-3.0, -50.0, max_h), "中段向下应吃掉");
        assert!(should_consume_scroll(3.0, -50.0, max_h), "中段向上应吃掉");

        // 顶部再往上滚：清单已没余量，放行给外层
        assert!(
            !should_consume_scroll(3.0, 0.0, max_h),
            "顶部继续上滚应放行"
        );
        // 顶部向下滚：有余量，吃掉
        assert!(should_consume_scroll(-3.0, 0.0, max_h), "顶部向下应吃掉");

        // 底部再往下滚：放行
        assert!(
            !should_consume_scroll(-3.0, -max_h, max_h),
            "底部继续下滚应放行"
        );
        // 底部向上滚：有余量，吃掉
        assert!(should_consume_scroll(3.0, -max_h, max_h), "底部向上应吃掉");

        // 内容没超框（max_h≈0）：一律放行，交给外层
        assert!(!should_consume_scroll(-3.0, 0.0, 0.0), "不可滚时应放行");
        assert!(!should_consume_scroll(3.0, 0.0, 0.0), "不可滚时应放行");

        // 横向滚轮（dy == 0）不拦
        assert!(!should_consume_scroll(0.0, -50.0, max_h), "横向滚轮应放行");
    }

    /// 无命中：提示必须明说「无需改动」，不能显示「将改动 0 句」这种让人以为坏了的
    /// 文案；预览与回执两种口径必须一致。
    #[test]
    fn min_gap_hint_reports_no_change_when_nothing_to_push() {
        let inert = GapPlan {
            times: Vec::new(),
            changed: 0,
            shortened: 0,
            unresolved: false,
        };
        let preview = min_gap_hint(&inert, false);
        let applied = min_gap_hint(&inert, true);
        assert!(preview.contains("无需改动"), "{preview}");
        assert_eq!(preview, applied, "无命中时预览与回执文案一致");
        assert!(!preview.contains("0 句"), "{preview}");
    }

    /// 有命中：预览说「将改动 N 句」、回执说「已推开 N 句」，句数必须与计划一致；
    /// 顺带把「收短了前句」如实带出来。
    #[test]
    fn min_gap_hint_counts_changed_sentences() {
        // 提示只读三个计数字段，`times` 与文案无关，这里不铺时间。
        let plan = GapPlan {
            times: Vec::new(),
            changed: 7,
            shortened: 2,
            unresolved: false,
        };
        let preview = min_gap_hint(&plan, false);
        let applied = min_gap_hint(&plan, true);
        assert!(preview.contains("将改动 7 句"), "{preview}");
        assert!(applied.contains("已推开 7 句"), "{applied}");
        assert!(applied.contains("2 句收短前句"), "{applied}");
    }

    /// 只有前一句被收短（后一句太短、抬起点会把它压没）时也要说清楚：起点没变，
    /// 但时间轴确实动了，不能落到「无需改动」那一支。
    #[test]
    fn min_gap_hint_reports_shortened_ends_without_start_moves() {
        let plan = GapPlan {
            times: Vec::new(),
            changed: 0,
            shortened: 3,
            unresolved: false,
        };
        let preview = min_gap_hint(&plan, false);
        let applied = min_gap_hint(&plan, true);
        assert!(preview.contains("将收短 3 句"), "{preview}");
        assert!(applied.contains("已收短 3 句"), "{applied}");
        assert!(!applied.contains("无需改动"), "{applied}");
    }

    /// 浮点近似比较（尺寸推导允许亚像素误差）
    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.05
    }

    #[test]
    fn fit_video_size_landscape_is_width_limited() {
        // 16:9 放进 800×700：宽度先触边，得到 800×450
        let (w, h) = fit_video_size(16.0 / 9.0, 800.0, 700.0);
        assert!(close(w, 800.0), "w={w}");
        assert!(close(h, 450.0), "h={h}");
    }

    #[test]
    fn fit_video_size_portrait_is_height_limited() {
        // 9:16 放进 800×700：高度先触边，宽度随之收缩，绝不溢出
        let (w, h) = fit_video_size(9.0 / 16.0, 800.0, 700.0);
        assert!(close(h, 700.0), "h={h}");
        assert!(close(w, 700.0 * 9.0 / 16.0), "w={w}");
        assert!(w <= 800.0 && h <= 700.0, "不得超出可用区域");
    }

    #[test]
    fn fit_video_size_square_fills_shorter_axis() {
        let (w, h) = fit_video_size(1.0, 640.0, 480.0);
        assert!(close(w, 480.0) && close(h, 480.0), "w={w} h={h}");
    }

    #[test]
    fn fit_video_size_rejects_degenerate_inputs() {
        assert_eq!(fit_video_size(0.0, 800.0, 700.0), (0.0, 0.0));
        assert_eq!(fit_video_size(-1.0, 800.0, 700.0), (0.0, 0.0));
        assert_eq!(fit_video_size(f32::NAN, 800.0, 700.0), (0.0, 0.0));
        assert_eq!(fit_video_size(f32::INFINITY, 800.0, 700.0), (0.0, 0.0));
        assert_eq!(fit_video_size(16.0 / 9.0, 0.0, 700.0), (0.0, 0.0));
        assert_eq!(fit_video_size(16.0 / 9.0, 800.0, -10.0), (0.0, 0.0));
    }

    #[test]
    fn monitor_screen_size_subtracts_split_and_chrome() {
        // 1920×1080、非处理中、左右二分、右栏 400：
        // 宽 = 1920 - 400 - 5 = 1515；高 = 1080 - 38(标题) - 134(时间轴) - 36(标头) - 52(控制条) = 820
        let (w, h) = monitor_screen_size(1920.0, 1080.0, false, false, 400.0);
        assert!(close(w, 1515.0), "w={w}");
        assert!(close(h, 820.0), "h={h}");
    }

    #[test]
    fn monitor_screen_size_subtracts_banner_while_processing() {
        let (_, h_idle) = monitor_screen_size(1920.0, 1080.0, false, false, 400.0);
        let (_, h_busy) = monitor_screen_size(1920.0, 1080.0, true, false, 400.0);
        assert!(
            close(h_idle - h_busy, Theme::BANNER_H),
            "处理中应让出横幅高度"
        );
    }

    #[test]
    fn monitor_screen_size_stacked_uses_full_width_and_fixed_height() {
        let (w, h) = monitor_screen_size(700.0, 1000.0, false, true, 400.0);
        assert!(close(w, 700.0), "堆叠布局下监视器占整行宽度");
        assert!(
            close(
                h,
                EDITOR_STACK_MONITOR_H - Theme::HEADER_H_SM - Theme::CONTROL_BAR_H
            ),
            "h={h}"
        );
    }

    #[test]
    fn monitor_screen_size_never_returns_negative() {
        let (w, h) = monitor_screen_size(100.0, 50.0, true, false, 400.0);
        assert_eq!((w, h), (0.0, 0.0), "极小视口下退化为 0，交由降级分支处理");
    }

    /// 画面按宽高比内接后，**至少有一个方向顶满**视口内屏——黑边只由宽高比造成。
    ///
    /// 回归护栏：这个系数曾经是 0.78。16:9 视频放进宽视口本来就左右留黑边，再乘
    /// 0.78 就变成**四周一圈**黑框、画面只剩视口一半多点，字小到看不清，用户直接
    /// 问「这个视频四周怎么这么多黑框」。
    #[test]
    fn monitor_video_fills_viewport_on_the_limiting_axis() {
        let screen = monitor_screen_size(1920.0, 1080.0, false, false, 400.0);
        let (w, h) = fit_video_size(
            16.0 / 9.0,
            screen.0 * MONITOR_VIDEO_FIT_FRAC,
            screen.1 * MONITOR_VIDEO_FIT_FRAC,
        );
        assert!(w > 0.0 && h > 0.0);
        assert!(
            w <= screen.0 + 0.01 && h <= screen.1 + 0.01,
            "内接结果不得溢出视口内屏：{w}×{h} ⊆ {screen:?}"
        );
        assert!(
            (w - screen.0).abs() < 0.01 || (h - screen.1).abs() < 0.01,
            "至少一个方向顶满（16:9 在 {screen:?} 内接得 {w}×{h}）"
        );
    }

    /// 方向对调：竖屏素材放进宽视口时应当是**高度**顶满、只在左右留黑边。
    #[test]
    fn monitor_video_letterboxes_only_on_the_other_axis() {
        let screen = monitor_screen_size(1920.0, 1080.0, false, false, 400.0);
        let (w, h) = fit_video_size(
            9.0 / 16.0,
            screen.0 * MONITOR_VIDEO_FIT_FRAC,
            screen.1 * MONITOR_VIDEO_FIT_FRAC,
        );
        assert!(
            (h - screen.1).abs() < 0.01,
            "竖屏素材高度顶满：{h} vs {screen:?}"
        );
        assert!(
            w < screen.0,
            "宽度方向的黑边由宽高比决定：{w} vs {screen:?}"
        );
    }

    /// 回归（P2）：「正在播放」行高亮必须用半开区间 `[start, end)`，与
    /// `AppState::get_active_segment()` 同源——共享边界时间属于**下一句**。
    /// 修前列表用的是闭区间 `<= seg.end`，相邻两句在边界处会同时高亮，
    /// 而监视器/预览只显示后一句，列表与画面不一致。
    #[test]
    fn playing_highlight_uses_half_open_interval() {
        // 相邻两句共享边界 t = 5.0：前句 end == 后句 start
        assert!(is_playing_at(0.0, 5.0, 0.0), "起点（左闭）应高亮");
        assert!(is_playing_at(0.0, 5.0, 3.0), "句中应高亮");
        assert!(
            !is_playing_at(0.0, 5.0, 5.0),
            "共享边界属于下一句，前句不得高亮（右开）"
        );
        assert!(is_playing_at(5.0, 9.0, 5.0), "共享边界处由后句高亮");
        assert!(!is_playing_at(5.0, 9.0, 9.0), "末句结束后不再高亮");
        assert!(!is_playing_at(0.0, 5.0, -0.1), "区间之外不得高亮");
    }

    /// 回归：多轨时间轴的片段「激活」判据必须与列表高亮同源（半开区间）。
    /// 修前这里写的是闭区间 `cur_time <= seg.end`，相邻两句在共享边界处会
    /// **同时**被点亮，而监视器只显示后一句——时间轴与画面/列表三者不一致。
    #[test]
    fn timeline_clip_active_matches_playing_judgement() {
        // 复用权威判据：直接断言半开区间在边界两侧的归属。
        assert!(is_playing_at(0.0, 5.0, 0.0), "起点归属前句");
        assert!(!is_playing_at(0.0, 5.0, 5.0), "共享边界不归属前句");
        assert!(is_playing_at(5.0, 9.0, 5.0), "共享边界归属后句");
        // 零长片段（start == end）半开区间恒为 false：既不会同时点亮相邻两句，
        // 也不存在「点不亮」之外的第三种状态。
        assert!(!is_playing_at(3.0, 3.0, 3.0), "零长片段不得激活");
    }

    // ==================== 监视器逐词高亮（卡拉 OK） ====================

    /// 测试用词级时间戳构造：`(词文本, start, end)`。
    fn words_of(words: &[(&str, f64, f64)]) -> Vec<WordTiming> {
        words
            .iter()
            .map(|(word, start, end)| WordTiming {
                word: (*word).to_string(),
                start: *start,
                end: *end,
            })
            .collect()
    }

    /// 逐词推进：`now` 落在第 2 个词中间时，第 1 个词整词「已读」、第 2 个词整词
    /// 「当前」，第 3 个词（尚未开始）不产生任何区间。
    ///
    /// 词级时间戳没有亚词精度，无法表达「词内读到 40%」，因此第 1 词必然是整词已读
    /// 而不是部分已读；第 2 词用「当前」而不是「已读」，否则高亮会提前一整词。
    /// 词文本沿用 whisper token 的带前导空格形态，比对必须按 `trim()` 后的内容命中。
    #[test]
    fn karaoke_marks_progresses_word_by_word() {
        let words = words_of(&[(" 今天", 0.0, 1.0), ("天气", 1.0, 2.0), (" 很好", 2.0, 3.0)]);
        let marks = karaoke_word_marks("今天天气很好", &words, 1.5, 0.0, 3.0);
        assert_eq!(
            marks,
            vec![(0..6, WordMark::Read), (6..12, WordMark::Current)],
            "第 2 词进行中：前词已读、当前词当前，后续词无区间"
        );
    }

    /// 边界与退化输入：`now` 早于段起点、词表为空、文本为空、时间非有限、段区间
    /// 非法都不得产生标记（不 panic、不高亮），调用方据此退回纯文本。
    /// 词表内单条词的时间非法时只跳过该词，不影响其余词的对齐与标记。
    #[test]
    fn karaoke_marks_out_of_range_or_degenerate_inputs_are_empty() {
        let words = words_of(&[("你好", 0.0, 1.0), ("世界", 1.0, 2.0)]);
        assert!(karaoke_word_marks("你好世界", &words, -0.1, 0.0, 2.0).is_empty());
        assert!(karaoke_word_marks("你好世界", &[], 0.5, 0.0, 2.0).is_empty());
        assert!(karaoke_word_marks("", &words, 0.5, 0.0, 2.0).is_empty());
        assert!(karaoke_word_marks("你好世界", &words, f64::NAN, 0.0, 2.0).is_empty());
        assert!(karaoke_word_marks("你好世界", &words, 0.5, 2.0, 2.0).is_empty());
        let bad_time = words_of(&[("你好", f64::NAN, f64::NAN), ("世界", 1.0, 2.0)]);
        assert_eq!(
            karaoke_word_marks("你好世界", &bad_time, 1.5, 0.0, 2.0),
            vec![(6..12, WordMark::Current)]
        );
    }

    /// 边界：`now` 晚于段终点（或正好等于最后时间戳的半开区间右界）时，所有词都算
    /// 「已读完」，不会留下一个永远处于「当前」的词。
    #[test]
    fn karaoke_marks_after_end_marks_all_words_read() {
        let words = words_of(&[("你好", 0.0, 1.0), ("世界", 1.0, 2.0)]);
        assert_eq!(
            karaoke_word_marks("你好世界", &words, 2.0, 0.0, 2.0),
            vec![(0..6, WordMark::Read), (6..12, WordMark::Read)],
            "now == 最后词 end：整句读完（半开区间右界）"
        );
        assert_eq!(
            karaoke_word_marks("你好世界", &words, 99.0, 0.0, 2.0),
            vec![(0..6, WordMark::Read), (6..12, WordMark::Read)]
        );
    }

    /// 容错：用户编辑过句子、词表还是旧识别的（顺序对不齐）→ 整体退化为无高亮，
    /// 而不是把某个词硬画到别处；返回的区间永远落在 char boundary 上，绝不 panic。
    #[test]
    fn karaoke_marks_misaligned_text_degrades_without_panic() {
        let words = words_of(&[("今天", 0.0, 1.0), ("天气", 1.0, 2.0)]);
        // 第 1 个词命中、第 2 个词找不到：整句放弃高亮
        assert!(karaoke_word_marks("今天情况尚可", &words, 1.5, 0.0, 3.0).is_empty());
        // 与词表完全无关的文本同样安全
        let text = "完全不同的内容";
        let marks = karaoke_word_marks(text, &words, 1.5, 0.0, 3.0);
        assert!(marks.is_empty());
        for (range, _) in &marks {
            assert!(text.is_char_boundary(range.start));
            assert!(text.is_char_boundary(range.end));
        }
        // 词的顺序颠倒（后词先出现）同样按「对不齐」处理，不得乱序产出区间
        let reversed = words_of(&[("世界", 0.0, 1.0), ("你好", 1.0, 2.0)]);
        assert!(karaoke_word_marks("你好世界", &reversed, 1.5, 0.0, 2.0).is_empty());
    }

    /// 多字节文本（中文 + emoji）的区间必须落在 char boundary 上：emoji 是 4 字节，
    /// 按字节切片时最容易踩到「半个字符」。
    #[test]
    fn karaoke_marks_land_on_char_boundaries_for_multibyte_text() {
        let text = "今天 😀 很好";
        let words = words_of(&[("今天", 0.0, 1.0), ("😀", 1.0, 2.0), ("很好", 2.0, 3.0)]);
        assert_eq!(
            karaoke_word_marks(text, &words, 1.5, 0.0, 3.0),
            vec![(0..6, WordMark::Read), (7..11, WordMark::Current)]
        );
        let marks = karaoke_word_marks(text, &words, 2.5, 0.0, 3.0);
        assert_eq!(
            marks,
            vec![
                (0..6, WordMark::Read),
                (7..11, WordMark::Read),
                (12..18, WordMark::Current)
            ]
        );
        for (range, _) in &marks {
            assert!(text.is_char_boundary(range.start), "区间起点必须在字符边界");
            assert!(text.is_char_boundary(range.end), "区间终点必须在字符边界");
        }
    }

    /// 双语导出（`译文\n原文`）：原文行起点 = 译文行字节数 + 1（换行符），词级高亮
    /// 只叠加在原文行内、不碰译文行；译文与原文去重成单行时偏移回到 0。
    #[test]
    fn bilingual_highlights_land_in_raw_line_only() {
        let mut seg = Segment::new(1, 0.0, 2.0, "今天");
        seg.translation = Some("Today".to_string());
        seg.words = words_of(&[("今天", 0.0, 1.0)]);
        let display = seg.export_text(ExportMode::Bilingual);
        assert_eq!(display, "Today\n今天");
        let base = raw_line_byte_start(&display, seg.display_text(), ExportMode::Bilingual)
            .expect("双语导出必须能定位原文行");
        assert_eq!(base, "Today\n".len());
        // 原文行内的区间叠加 base 后仍落在原文行内（不越界到译文行）
        let marks = karaoke_word_marks(seg.display_text(), &seg.words, 0.5, seg.start, seg.end);
        assert_eq!(marks, vec![(0..6, WordMark::Current)]);
        for (range, _) in &marks {
            assert!(base + range.start >= "Today\n".len());
            assert!(base + range.end <= display.len());
        }
        // 译文与原文一致（中翻中）时 export_text 去重成单行：按原文整段处理
        let mut same = seg.clone();
        same.translation = Some("今天".to_string());
        let dedup = same.export_text(ExportMode::Bilingual);
        assert_eq!(dedup, "今天");
        assert_eq!(
            raw_line_byte_start(&dedup, same.display_text(), ExportMode::Bilingual),
            Some(0)
        );
    }

    /// 仅译文模式：文本行是译文（没有词级时间），返回 `None` 不做逐词高亮；
    /// 译文缺失时 `export_text` 回退原文，此时仍按原文行高亮；原文模式恒为整段。
    #[test]
    fn translation_only_mode_has_no_word_highlight() {
        let mut seg = Segment::new(1, 0.0, 2.0, "今天");
        seg.translation = Some("Today".to_string());
        let display = seg.export_text(ExportMode::TranslationOnly);
        assert_eq!(display, "Today");
        assert_eq!(
            raw_line_byte_start(&display, seg.display_text(), ExportMode::TranslationOnly),
            None
        );
        assert_eq!(
            raw_line_byte_start(&display, seg.display_text(), ExportMode::RawOnly),
            Some(0)
        );
        let mut fallback = seg.clone();
        fallback.translation = None;
        let text = fallback.export_text(ExportMode::TranslationOnly);
        assert_eq!(text, "今天");
        assert_eq!(
            raw_line_byte_start(&text, fallback.display_text(), ExportMode::TranslationOnly),
            Some(0)
        );
    }

    /// 样式语义：当前词换强调色 + 加粗；已读词只加浅底，**不动**字色与字重
    /// （覆盖层字色 / 字号属于用户字幕配置，必须原样保留）。
    #[test]
    fn karaoke_styles_emphasize_current_and_wash_read_words() {
        let current = karaoke_mark_style(WordMark::Current);
        assert!(current.color.is_some(), "当前词必须有强调色");
        assert_eq!(current.font_weight, Some(FontWeight::BOLD));
        assert!(current.background_color.is_none());

        let read = karaoke_mark_style(WordMark::Read);
        assert!(read.background_color.is_some(), "已读词加浅底");
        assert!(read.color.is_none(), "已读词不得覆盖用户字色");
        assert!(read.font_weight.is_none(), "已读词不得覆盖用户字重");
    }

    /// 帧解码助手必须交出 **BGRA**：GPUI 图集纹理是 B8G8R8A8，而 `image` crate 给的是
    /// RGBA。搞反了就是红蓝互换（人脸变蓝、天空变橙），而且只在静态定位帧上出现、
    /// 播放时又正常——这种「一半对一半错」的配色 bug 极难靠肉眼定位，所以钉死字节序。
    #[test]
    fn frame_decode_helper_returns_bgra_bytes() {
        let path = std::env::temp_dir().join(format!("v2w_frame_bgra_{}.png", std::process::id()));
        let mut img = image::RgbaImage::new(2, 2);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([255, 0, 0, 255]); // 纯红
        }
        img.save(&path).expect("写出测试图");

        let decoded = render_image_from_frame_file(&path).expect("正常图片应解码成功");
        let bytes = decoded.as_bytes(0).expect("应有一帧");
        assert_eq!(
            &bytes[0..4],
            &[0, 0, 255, 255],
            "红色像素在 BGRA 里应是 00 00 FF FF"
        );
        assert_eq!(bytes.len(), 2 * 2 * 4, "像素数不该被改动");

        let _ = std::fs::remove_file(&path);
    }

    /// 坏文件 / 不存在的路径只返回 None，绝不能 panic：调用点在后台抽帧线程里，
    /// panic 会连累整条抽帧循环，时间轴就此不再更新画面。
    #[test]
    fn frame_decode_helper_degrades_without_panicking() {
        let path = std::env::temp_dir().join(format!("v2w_frame_bad_{}.jpg", std::process::id()));
        std::fs::write(&path, b"not an image at all").unwrap();
        assert!(
            render_image_from_frame_file(&path).is_none(),
            "非图片应返回 None"
        );
        assert!(
            render_image_from_frame_file(std::path::Path::new("Z:/definitely/missing.jpg"))
                .is_none(),
            "不存在的路径应返回 None"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 监视器画面来源判据：播放器「有画面」时，同版本的第二次渲染必须复用已建好的
    /// 纹理——掉回静态帧/占位会在稳定播放与暂停定格时整帧闪一下；而 `stop()` 之后
    /// （`has_live_picture=false`）即使 `cached_live_image` 里还留着旧纹理，
    /// 也必须回到静态定位帧链路，否则跳转后会看到跳转前的旧画面。
    #[test]
    fn monitor_picture_source_pins_live_reuse_and_stop_fallback() {
        // 取到新版本帧：重建（唯一一次 take，之后槽位为空）
        assert_eq!(
            monitor_picture_source(true, true, true),
            MonitorPictureSource::LiveNewFrame
        );
        assert_eq!(
            monitor_picture_source(true, true, false),
            MonitorPictureSource::LiveNewFrame
        );
        // 同版本第二次渲染：复用缓存纹理，不得落回静态帧
        assert_eq!(
            monitor_picture_source(true, false, true),
            MonitorPictureSource::LiveReuse
        );
        // 有画面但没纹理（首帧未到/缓存缺失）：留空等下一帧，也不落静态帧
        assert_eq!(
            monitor_picture_source(true, false, false),
            MonitorPictureSource::LivePending
        );
        // stop() 之后：播放器无画面，缓存里残留的旧纹理不得拦住静态帧链路
        assert_eq!(
            monitor_picture_source(false, false, true),
            MonitorPictureSource::Static
        );
        assert_eq!(
            monitor_picture_source(false, true, false),
            MonitorPictureSource::Static
        );
    }

    /// 改造前 `render_multitrack_timeline` 里那段判据的参照物：片段数 ≤ 20 时全画，
    /// 否则按「选中句，或与 ±15 秒窗口相交」过滤，输出存储顺序。
    ///
    /// 只服务于比对，不进生产代码；三条分支（含被 ±margin 包含的 `cur ∈ [start, end]`）
    /// 逐字照抄改造前的实现。
    fn legacy_timeline_clip_positions(
        segments: &[Segment],
        current_time: f64,
        selected: Option<usize>,
    ) -> Vec<usize> {
        if segments.len() <= 20 {
            return (0..segments.len()).collect();
        }
        segments
            .iter()
            .enumerate()
            .filter(|(_, seg)| {
                selected == Some(seg.index)
                    || (current_time >= seg.start && current_time <= seg.end)
                    || (current_time >= seg.start - 15.0 && current_time <= seg.end + 15.0)
            })
            .map(|(pos, _)| pos)
            .collect()
    }

    /// 渲染路径真正调用的入口必须与改造前的两分支逻辑逐点等价：≤ 20 句全画、
    /// 否则走 ±15 秒窗口；20 / 21 句两份表覆盖两侧分支。
    #[test]
    fn timeline_clip_positions_matches_legacy_two_branch_filter() {
        let small: Vec<Segment> = (0..20)
            .map(|i| Segment::new(i + 1, i as f64 * 10.0, i as f64 * 10.0 + 2.0, "句"))
            .collect();
        let big: Vec<Segment> = (0..21)
            .map(|i| Segment::new(i + 1, i as f64 * 10.0, i as f64 * 10.0 + 2.0, "句"))
            .collect();
        for segments in [small, big] {
            let index = build_timeline_index(&segments);
            for selected in [None, Some(1usize), Some(21usize)] {
                let mut t = -30.0f64;
                while t <= 260.0 {
                    assert_eq!(
                        timeline_clip_positions(&segments, &index, t, selected),
                        legacy_timeline_clip_positions(&segments, t, selected),
                        "片段数 {} t={t} selected={selected:?}",
                        segments.len()
                    );
                    t += 1.0;
                }
            }
        }
    }

    /// 绘制顺序必须是存储顺序（索引是按 start 排过的，取出的下标要先还原），
    /// 且选中句同时落在窗口内时不得画两遍——旧实现只可能命中一次。
    #[test]
    fn timeline_clip_positions_restores_storage_order_without_duplicates() {
        // 存储顺序与时间顺序**相反**：忘了还原顺序，画出来的块就会整体颠倒
        let segments: Vec<Segment> = (0..30)
            .map(|i| {
                let start = (30 - i) as f64;
                Segment::new(i + 1, start, start + 0.5, "句")
            })
            .collect();
        let index = build_timeline_index(&segments);

        // t = 15：窗口 [0, 30] 正好罩住全部 30 句，选中句（index 3）也在窗口内
        let positions = timeline_clip_positions(&segments, &index, 15.0, Some(3));
        assert_eq!(positions, (0..segments.len()).collect::<Vec<_>>());
        let selected_pos = segments
            .iter()
            .position(|s| s.index == 3)
            .expect("表里有 3");
        assert_eq!(
            positions.iter().filter(|&&p| p == selected_pos).count(),
            1,
            "选中句同时在窗口内时只能画一次：{positions:?}"
        );

        // 选中句远在窗口外时依然要画（旧实现无条件画它）
        let far = timeline_clip_positions(&segments, &index, 100.0, Some(30));
        assert_eq!(far, vec![29usize], "窗口内没有别的片段，只剩选中句");
    }

    /// ±15 秒窗口只允许有一个来源：窗口常量就是渲染路径用的那个值（旧实现把它
    /// 写死在过滤闭包里），同时钉住「片段数 ≤ 阈值时全画」的另一半判据。
    #[test]
    fn timeline_window_margin_constant_is_the_single_source() {
        assert!((TIMELINE_WINDOW_MARGIN_SECS - 15.0).abs() < f64::EPSILON);
        assert_eq!(TIMELINE_FULL_DRAW_LIMIT, 20);

        // 21 句（越过全画阈值）里只有一句恰好压在 +15 秒边界上
        let mut segments: Vec<Segment> = (0..19)
            .map(|i| {
                Segment::new(
                    i + 1,
                    100.0 + i as f64 * 10.0,
                    105.0 + i as f64 * 10.0,
                    "远离窗口",
                )
            })
            .collect();
        segments.push(Segment::new(
            20,
            TIMELINE_WINDOW_MARGIN_SECS,
            TIMELINE_WINDOW_MARGIN_SECS,
            "零长度：恰好压在窗口右界",
        ));
        segments.push(Segment::new(
            21,
            TIMELINE_WINDOW_MARGIN_SECS + 0.001,
            TIMELINE_WINDOW_MARGIN_SECS + 0.001,
            "零长度：刚超出窗口右界",
        ));
        let index = build_timeline_index(&segments);
        assert_eq!(
            timeline_clip_positions(&segments, &index, 0.0, None),
            vec![19usize],
            "只有恰好压界那句可见，刚超界与远处的都不可见"
        );
        // 选中那句刚超界的：窗口扫描取不到它，但「选中句恒可见」必须把它补上
        assert_eq!(
            timeline_clip_positions(&segments, &index, 0.0, Some(21)),
            vec![19usize, 20],
            "选中句必须在绘制集合里，且仍按存储顺序"
        );
    }

    /// 默认 zoom = 1.0 时窗口必须是整段 `[0, total]`：这是「与改造前逐像素一致」的根
    /// ——整段视图下窗口一旦整体偏移，所有按时间定位的元素都会跟着错位。
    #[test]
    fn window_at_default_zoom_covers_the_whole_timeline() {
        let win = timeline_window(1.0, 0.0, 120.0);
        assert_eq!(
            win,
            TimelineWindow {
                start: 0.0,
                end: 120.0
            }
        );
        // 未放大时平移没有意义，一律被夹回 [0, total]
        assert_eq!(timeline_window(1.0, 30.0, 120.0).start, 0.0);
        assert_eq!(timeline_window(1.0, -5.0, 120.0).start, 0.0);
        // 比例换算必须与改造前的 `t / total` **逐位**一致（不是近似）
        assert_eq!(win.time_to_ratio(42.0), 42.0 / 120.0);
        assert_eq!(win.ratio_to_time(0.25), 30.0);
    }

    /// 缩放的语义不变量：锚点（鼠标指着的那一帧）缩放前后必须落在同一个像素上。
    #[test]
    fn zoom_keeps_the_anchor_time_in_place() {
        let total = 100.0;
        let (zoom, start) = zoom_at(0.25, 0.0, total, 1.0, 4.0);
        assert!((zoom - 4.0).abs() < 1e-9, "zoom = {zoom}");
        assert!((start - 18.75).abs() < 1e-9, "start = {start}");
        let win = timeline_window(zoom, start, total);
        assert!((win.time_to_ratio(25.0) - 0.25).abs() < 1e-9);

        // 已经平移过的窗口：锚点照样不动
        let panned = timeline_window(2.0, 50.0, 200.0);
        assert_eq!((panned.start, panned.end), (50.0, 150.0));
        let anchor = 0.8;
        let anchor_time = panned.ratio_to_time(anchor);
        let (zoom, start) = zoom_at(anchor, panned.start, 200.0, 2.0, 2.0);
        let zoomed = timeline_window(zoom, start, 200.0);
        assert!((zoomed.time_to_ratio(anchor_time) - anchor).abs() < 1e-9);
    }

    /// 平移夹在 `[0, total]`：窗口不能被拖出素材范围，落界内时原样保留。
    #[test]
    fn pan_window_is_clamped_into_the_media_range() {
        // 放大 4 倍 → 窗口宽 25 秒，起点上限 = 100 - 25
        let right = timeline_window(4.0, 90.0, 100.0);
        assert_eq!((right.start, right.end), (75.0, 100.0));
        let left = timeline_window(4.0, -30.0, 100.0);
        assert_eq!((left.start, left.end), (0.0, 25.0));
        let middle = timeline_window(4.0, 40.0, 100.0);
        assert_eq!((middle.start, middle.end), (40.0, 65.0));
    }

    /// 倍率夹在 `[1, 20]`：滚轮连打既不会把窗口压成 0 宽，也不会让窗口比整段还宽；
    /// 缩回整段时窗口必须回到 `[0, total]`。
    #[test]
    fn zoom_factor_is_clamped_to_the_configured_range() {
        let (zoom, _) = zoom_at(0.5, 0.0, 60.0, 15.0, 10.0);
        assert_eq!(zoom, TIMELINE_ZOOM_MAX);
        let (zoom, start) = zoom_at(0.5, 20.0, 60.0, 1.5, 0.01);
        assert_eq!(zoom, TIMELINE_ZOOM_MIN);
        assert_eq!(start, 0.0, "缩回整段后窗口起点必须回到 0");
        // 非有限倍率一律退回整段，不让 NaN/Inf 渗进布局
        assert_eq!(clamp_zoom(f64::NAN), TIMELINE_ZOOM_MIN);
        assert_eq!(clamp_zoom(f64::INFINITY), TIMELINE_ZOOM_MIN);
    }

    /// 播放跟随：放大后游标移出可视带时窗口跟着走；整段视图（zoom = 1）下一条都不改，
    /// 否则就会破坏「默认视图与改造前逐像素一致」。
    #[test]
    fn playhead_follow_only_moves_a_zoomed_window() {
        let total = 100.0;
        let whole = timeline_window(1.0, 0.0, total);
        // 播放中游标在 42% / 99% / 越界到 150 秒，都不该动整段视图
        assert_eq!(follow_playhead(whole, 42.0, total, true, None), whole);
        assert_eq!(follow_playhead(whole, 99.0, total, true, None), whole);
        assert_eq!(follow_playhead(whole, 150.0, total, true, None), whole);

        // 放大 4 倍 → 窗口宽 25 秒
        let win = timeline_window(4.0, 0.0, total);
        // 播放中游标越过 90% 处：窗口被推着走，游标停在 90% 位置
        let followed = follow_playhead(win, 24.0, total, true, None);
        assert!((followed.start - (24.0 - 25.0 * 0.9)).abs() < 1e-9);
        // 暂停、且没手动动过视图：游标还在窗口里就不动
        assert_eq!(follow_playhead(win, 24.0, total, false, None), win);
        // 暂停、且没手动动过视图：游标真的出了窗口才跟随（兜底）
        let followed = follow_playhead(win, 40.0, total, false, None);
        assert!((followed.start - 15.0).abs() < 1e-9);
    }

    /// 回归（P1）：暂停时手动平移过视图，窗口必须保持用户给的起点——播放头允许离屏，
    /// 不能被下一帧的跟随悄悄夹回来。旧实现把「暂停」写成 `band = 0` 的同一条分支，
    /// 看似只取消了前后留白，其实还留着「窗口必须包住播放头」这条硬约束，于是
    /// `current_time` 还贴在 0 时向右平移等于白滚（滚轮/平移完全没反应）。
    /// 播放头一动（seek / 播放）标记自动失效，跟随立即恢复。
    #[test]
    fn paused_manual_pan_is_not_yanked_back_by_the_playhead() {
        let total = 100.0;
        // 4 倍缩放后窗口宽 25 秒；用户把它平移到 [10, 35]
        let panned = timeline_window(4.0, 10.0, total);
        assert!((panned.start - 10.0).abs() < 1e-9, "平移本身要生效");
        // 播放头还在 0（窗口外）：窗口一点都不许动
        assert_eq!(
            follow_playhead(panned, 0.0, total, false, Some(0.0)),
            panned
        );
        // 用户 seek 到窗口外的句子：标记失效，窗口跟过去把播放头带回视野
        let followed = follow_playhead(panned, 60.0, total, false, Some(0.0));
        assert!(
            followed.start <= 60.0 && followed.end >= 60.0,
            "seek 之后播放头必须可见：{followed:?}"
        );
        // 播放中永远跟随（游标贴到右侧留白带就推着窗口走）
        let playing = follow_playhead(panned, 34.0, total, true, Some(34.0));
        assert!(playing.start > 10.0, "播放时必须跟随：{playing:?}");
    }

    /// 没有素材（总时长 0）、极短素材、非有限输入：缩放/平移/换算都不许 panic，
    /// 窗口退化成空或按整段处理。
    #[test]
    fn degenerate_durations_and_inputs_do_not_panic() {
        for total in [0.0, -1.0, 1e-12, 0.4] {
            let win = timeline_window(8.0, 5.0, total);
            assert!(win.end >= win.start, "total = {total} 时窗口不能反向");
            assert!(win.time_to_ratio(1.0).is_finite(), "total = {total}");
            assert!(win.ratio_to_time(0.5).is_finite(), "total = {total}");
            let (zoom, start) = zoom_at(0.5, 3.0, total, 2.0, 3.0);
            assert!(zoom.is_finite() && start.is_finite(), "total = {total}");
        }
        let (zoom, start) = zoom_at(f64::NAN, f64::NAN, 10.0, 2.0, f64::NAN);
        assert!(zoom.is_finite() && start.is_finite());
    }

    /// 拖拽左端：0 下界、前一句边界、最小时长三条夹紧，外加非有限输入不 panic。
    #[test]
    fn trim_start_clamps_zero_previous_end_and_min_duration() {
        let f = |raw: f64| trim_start(raw, 10.0, 12.0, Some(8.0), 60.0, TIMELINE_TRIM_MIN_SECS);
        assert!((f(8.5) - 8.5).abs() < 1e-9, "区间内原样通过");
        assert!((f(-3.0) - 8.0).abs() < 1e-9, "不许拖到前一句之前");
        assert!((f(11.99) - 11.9).abs() < 1e-9, "最小时长 0.1s 是硬下界");
        assert!((f(f64::NAN) - 10.0).abs() < 1e-9, "NaN 退回原值");
        assert!((f(f64::INFINITY) - 10.0).abs() < 1e-9, "∞ 退回原值");
        assert!((f(f64::NEG_INFINITY) - 10.0).abs() < 1e-9, "-∞ 退回原值");
        // 第一句没有前邻居：只能拖到 0
        let first = trim_start(-5.0, 1.0, 3.0, None, 60.0, TIMELINE_TRIM_MIN_SECS);
        assert!(first.abs() < 1e-9, "first = {first}");
    }

    /// 拖拽右端：最小时长下界 + 后一句起点 / 总时长两条上界。
    #[test]
    fn trim_end_clamps_min_duration_next_start_and_total() {
        let f = |raw: f64| trim_end(raw, 10.0, 12.0, Some(15.0), 60.0, TIMELINE_TRIM_MIN_SECS);
        assert!((f(14.0) - 14.0).abs() < 1e-9, "区间内原样通过");
        assert!((f(9.0) - 10.1).abs() < 1e-9, "不许压到最小时长以下");
        assert!((f(30.0) - 15.0).abs() < 1e-9, "不许压过后一句起点");
        assert!((f(f64::NAN) - 12.0).abs() < 1e-9, "NaN 退回原值");
        // 最后一句：上界只剩总时长
        let last = trim_end(90.0, 50.0, 55.0, None, 60.0, TIMELINE_TRIM_MIN_SECS);
        assert!((last - 60.0).abs() < 1e-9, "last = {last}");
    }

    /// 整段平移：时长不变、被边界截住、非有限输入原地不动。
    #[test]
    fn shift_segment_keeps_duration_and_stops_at_neighbours() {
        let p = |d: f64| {
            shift_segment(
                d,
                10.0,
                12.0,
                Some(8.0),
                Some(15.0),
                60.0,
                TIMELINE_TRIM_MIN_SECS,
            )
        };
        let (s, e) = p(1.0);
        assert!((s - 11.0).abs() < 1e-9 && (e - 13.0).abs() < 1e-9);
        assert!((e - s - 2.0).abs() < 1e-9, "时长必须不变");
        let (s, e) = p(-100.0);
        assert!(
            (s - 8.0).abs() < 1e-9 && (e - 10.0).abs() < 1e-9,
            "左越界被前一句截住"
        );
        let (s, e) = p(100.0);
        assert!(
            (s - 13.0).abs() < 1e-9 && (e - 15.0).abs() < 1e-9,
            "右越界被后一句截住"
        );
        // 最后一句：被总时长截住，时长仍是 5 秒
        let (s, e) = shift_segment(100.0, 50.0, 55.0, None, None, 60.0, TIMELINE_TRIM_MIN_SECS);
        assert!((s - 55.0).abs() < 1e-9 && (e - 60.0).abs() < 1e-9);
        // 非有限位移：原地不动，绝不产出 NaN 时间码
        let (s, e) = p(f64::NAN);
        assert!((s - 10.0).abs() < 1e-9 && (e - 12.0).abs() < 1e-9);
    }

    /// 顺序不变：左右端与整段平移到任意极值，都不会把这一句挪到前一句之前或压过后一句
    /// 起点，也不会越过 `[0, total]`；退化/非有限输入同样不产出 NaN。
    #[test]
    fn drag_edits_never_reorder_or_leave_the_timeline() {
        let (prev_end, next_start, total) = (8.0, 15.0, 60.0);
        for raw in [-1000.0, -1.0, 0.0, 8.0, 11.0, 14.9, 1e9] {
            let s = trim_start(
                raw,
                10.0,
                12.0,
                Some(prev_end),
                total,
                TIMELINE_TRIM_MIN_SECS,
            );
            assert!(s >= prev_end - 1e-9, "raw = {raw} 时拖到前一句之前：{s}");
            assert!(
                s <= 12.0 - TIMELINE_TRIM_MIN_SECS + 1e-9,
                "raw = {raw} 时短于最小时长：{s}"
            );
            let e = trim_end(
                raw,
                10.0,
                12.0,
                Some(next_start),
                total,
                TIMELINE_TRIM_MIN_SECS,
            );
            assert!(
                e >= 10.0 + TIMELINE_TRIM_MIN_SECS - 1e-9,
                "raw = {raw} 时短于最小时长：{e}"
            );
            assert!(e <= next_start + 1e-9, "raw = {raw} 时压过后一句：{e}");
        }
        for d in [-1e9, -1000.0, -5.0, 0.0, 0.3, 1000.0, 1e9, f64::NAN] {
            let (s, e) = shift_segment(
                d,
                10.0,
                12.0,
                Some(prev_end),
                Some(next_start),
                total,
                TIMELINE_TRIM_MIN_SECS,
            );
            assert!(s >= prev_end - 1e-9 && s >= 0.0, "d = {d} 时左越界：{s}");
            assert!(
                e <= next_start + 1e-9 && e <= total + 1e-9,
                "d = {d} 时右越界：{e}"
            );
            assert!(s.is_finite() && e.is_finite(), "d = {d} 产出非有限时间码");
        }
    }

    /// 退化输入（总时长 0 / 负 / 非有限，最小时长非法）不许产出 NaN 或反向区间。
    #[test]
    fn trim_edits_survive_degenerate_inputs() {
        for total in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let s = trim_start(5.0, 4.0, 6.0, None, total, TIMELINE_TRIM_MIN_SECS);
            assert!(
                s.is_finite() && s <= 6.0 - TIMELINE_TRIM_MIN_SECS + 1e-9,
                "total = {total} 的起点：{s}"
            );
            let e = trim_end(5.0, 4.0, 6.0, None, total, TIMELINE_TRIM_MIN_SECS);
            assert!(
                e.is_finite() && e >= 4.0 + TIMELINE_TRIM_MIN_SECS - 1e-9,
                "total = {total} 的终点：{e}"
            );
        }
        // 最小时长传了 NaN / 负值：退回常量照常工作，不 panic
        assert!((trim_start(5.0, 4.0, 6.0, None, 60.0, f64::NAN) - 5.0).abs() < 1e-9);
        assert!((trim_end(5.0, 4.0, 6.0, None, 60.0, -1.0) - 5.0).abs() < 1e-9);
    }

    /// 鼠标定位必须走 [`TimelineWindow`]（放大/平移后的可视窗口），不能拿整段总时长当分母。
    #[test]
    fn trim_mouse_position_maps_through_the_visible_window() {
        let win = timeline_window(4.0, 30.0, 120.0); // 窗口 = [30, 60]
        assert!((win.start - 30.0).abs() < 1e-9);
        let track = Theme::TRACK_LABEL_W + Theme::TRACK_RIGHT_PAD + 800.0;
        let mid = timeline_time_at_x(gpui::px(Theme::TRACK_LABEL_W + 400.0), gpui::px(track), win)
            .expect("轨道宽度足够时必须算得出时间");
        assert!((mid - 45.0).abs() < 1e-6, "mid = {mid}");
        // 轨道比阈值还窄：算不出来（不 panic，也不给半截换算）
        let narrow = Theme::TRACK_LABEL_W + Theme::TRACK_RIGHT_PAD;
        assert!(timeline_time_at_x(gpui::px(10.0), gpui::px(narrow), win).is_none());
    }
}
