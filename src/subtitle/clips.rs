//! 智能切片：从长视频的字幕里规划「高光片段」候选（纯函数规划器）。
//!
//! 产品形态是 OpusClip 式的「一键切高光」：长视频转写完成后，本模块从逐句字幕里
//! 挑出若干条候选片段（起点 / 终点 / 得分 / 理由 / 兜底标题），用户相中哪条就交给
//! [`crate::engines::ffmpeg`] 的 `plan_segment_clip` / `export_clip` 落地成短视频。
//! **本模块只做规划**：不碰 IO、不碰 FFmpeg、不碰界面，UI 与导出接线由调用方负责。
//!
//! # 为什么是纯函数
//!
//! 规划过程没有任何随机 / 全局状态 / 环境依赖：同样的 `&[Segment]` + 同样的
//! [`ClipOptions`] 必然得到逐字段一致的候选列表（连 `reason` 文案都一样）。
//! 这让全部行为可单测，也让「两次点击结果不一样」这类问题从根上不可能发生。
//!
//! # 与 chapters.rs 的关系
//!
//! 章节聚类（[`crate::utils::chapters::plan_chapters`]）**不**被本模块调用：
//! 切片规划不该被章节参数（目标时长、组数上限）绑架。但「话题开头」是高光的
//! 强信号（换话题时讲话的人往往先抛结论），所以调用方把章节起点经
//! [`ClipOptions::chapter_starts`] 传进来，只做**打分加成**、不做硬切分。
//!
//! # 算法总览（确定性，四步）
//!
//! 1. **候选窗**：以每个非空文本句的 start 为锚，向后吞句到 [min, max] 内的最长窗；
//! 2. **打分**：五个维度（提问密度 / 语速 / 置信度 / 章节 / 时长）加权求和归一化；
//! 3. **去重**：按得分降序贪心，重叠或间隔不足 `gap_secs` 的候选互相排斥；
//! 4. **输出**：按 start 升序（导出顺序即时间顺序）。
//!
//! 入参乱序也安全：内部按 `(start, index, end, text)` 全序排序**副本**处理，
//! 不修改调用方数据，且任何输入顺序都得到同一份结果。

use crate::subtitle::segment::Segment;
use crate::subtitle::stats::visible_char_count;

// ─────────────────────────────────────────────────────────────
// 参数
// ─────────────────────────────────────────────────────────────

/// 默认最多产出几条候选。
///
/// 5 条正好铺满「候选列表」一屏，用户挑一条的成本最低；再多会把「挑一条」
/// 变成翻页浏览，反而拖慢「一键切片」的主流程。
pub const DEFAULT_COUNT: usize = 5;

/// 单条切片最短时长（秒）。
///
/// 15 秒是「装得下一个完整话题」的下限（起因—结论至少要两三句话）；
/// 更短的切片在平台侧也会被当「碎片」压低推荐，切出来意义不大。
pub const DEFAULT_MIN_SECS: f64 = 15.0;

/// 单条切片最长时长（秒）。
///
/// 高光切片面向竖屏短视频：超过 90 秒就不再是「切片」而是「长视频搬运」，
/// 完播率骤降，平台也不再给「短内容」的流量加权。
pub const DEFAULT_MAX_SECS: f64 = 90.0;

/// 候选之间的最小时间间隔（秒）。
///
/// 两条候选首尾相接会抢同一段内容（同一句台词出现在两条切片里，用户导出后
/// 发现两条高度重复）；5 秒大致是一句话的长度，作为「呼吸空间」足够让相邻
/// 候选的内容互不重叠，又不至于把中间的过渡段浪费掉。
pub const DEFAULT_GAP_SECS: f64 = 5.0;

/// 规划参数。
///
/// 非法值（NaN / 负数 / 退化区间）不会 panic，也不会照单全收——`plan_highlight_clips`
/// 会把它们**逐字段退回默认值**（防御风格对齐 [`crate::utils::chapters::plan_chapters`]），
/// 见 [`Params::sanitized`]。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipOptions {
    /// 最多产出几条（默认 [`DEFAULT_COUNT`]）。`0` 是合法语义「一条都不产出」。
    pub count: usize,
    /// 单条最短时长（秒，默认 [`DEFAULT_MIN_SECS`]）。
    pub min_secs: f64,
    /// 单条最长时长（秒，默认 [`DEFAULT_MAX_SECS`]）。
    pub max_secs: f64,
    /// 候选之间最小间隔（秒，默认 [`DEFAULT_GAP_SECS`]）。
    pub gap_secs: f64,
    /// 章节起点（秒），可为空。只参与打分加成，不参与切分。
    pub chapter_starts: Vec<f64>,
}

impl Default for ClipOptions {
    fn default() -> Self {
        Self {
            count: DEFAULT_COUNT,
            min_secs: DEFAULT_MIN_SECS,
            max_secs: DEFAULT_MAX_SECS,
            gap_secs: DEFAULT_GAP_SECS,
            chapter_starts: Vec::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 结果
// ─────────────────────────────────────────────────────────────

/// 一条候选切片：可直接拿去规划导出的时间区间 + 人读的挑选依据。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipCandidate {
    /// 片段起点（秒）。对齐到句 `Segment::start`；负值已被按 0 处理。
    pub start: f64,
    /// 片段终点（秒）。对齐到句 `Segment::end`。
    ///
    /// 唯一例外：锚点句自己就超过 `max_secs` 的「巨句」被钳制到 `start + max_secs`
    ///（见 [`build_window_candidate`]）。宁可切在句中，也不让一条坏数据巨句产出
    /// 超长切片、或让这段内容在候选列表里凭空消失。
    pub end: f64,
    /// 0~1 归一化得分（五项加权和除以权重总和）。
    pub score: f64,
    /// 人读的得分来源摘要，如「疑问句×2、章节开头」。恒非空。
    pub reason: String,
    /// 覆盖的 `Segment::index`，按时间升序。与 `AppState::select_segment` 同一套编号，
    /// 可原样拿去定位。
    pub seg_indices: Vec<usize>,
    /// 兜底标题：首句文本截断 [`TITLE_MAX_CHARS`] 字（截断口径与
    /// [`crate::utils::chapters`] 的兜底标题一致：按字符截、去悬空标点）。
    pub title: String,
}

// ─────────────────────────────────────────────────────────────
// 打分常量表
// ─────────────────────────────────────────────────────────────

/// 权重：提问密度。五项里最重。
///
/// 疑问 / 感叹句是「观众会停下来看」的最强文本信号（hook）：OpusClip 类产品
///普遍把「悬念 / 提问」当第一排序特征，高光片段首先要能钩住人。
pub const WEIGHT_ENGAGEMENT: f64 = 0.35;

/// 权重：语速贴近度。
///
/// 语速合适是「看得下去」的底线，但它与内容好坏相关性弱（讲得快≠内容差），
/// 只作次要微调。
pub const WEIGHT_SPEECH_FIT: f64 = 0.15;

/// 权重：识别置信度。
///
/// 置信度直接决定这条切片的字幕质量——切出来字幕是错的，再好的内容也废了；
/// 但它是「不出错」而不是「出彩」，权重低于提问密度。
pub const WEIGHT_CONFIDENCE: f64 = 0.20;

/// 权重：章节开头加成。
///
/// 结构性信号（话题开头通常先抛结论），但调用方可能根本不传章节（缺省视为中性
/// 0.5），不该主导排序，给与语速同级的轻权重。
pub const WEIGHT_CHAPTER_BONUS: f64 = 0.15;

/// 权重：时长贴合度。
///
/// 时长偏好是平台属性而非内容属性（30~60 秒只是「更好发」），轻权重微调即可。
pub const WEIGHT_LENGTH_FIT: f64 = 0.15;

/// 情绪 / 悬念收尾字符：疑问、感叹、省略。
///
/// 省略号也算 hook——「结果没想到……」式的悬念收尾同样让人想看下去。
/// 只看**最后一个字符**：句中带问号的陈述句（「我问他去不去。」）不该被误判成提问。
const EXCITEMENT_ENDINGS: [char; 5] = ['?', '？', '!', '！', '…'];

/// 语速甜区（字符/秒）下缘：落在 [4, 8] 的窗得满分。
///
/// 中文口播的自然语速折算到字幕字符数大约每秒 4~8 字。对照
/// [`crate::subtitle::stats`] 的中文「舒适上限」9 CPS 留了一成余量：高光切片
/// 要「听得清、跟得上」，比「字幕合规」更严。
const CPS_SWEET_LOW: f64 = 4.0;

/// 语速甜区（字符/秒）上缘。
const CPS_SWEET_HIGH: f64 = 8.0;

/// 语速过慢侧的 0 分锚点（0 字/秒：窗内几乎只剩停顿，撑不起一条切片）。
const CPS_SLOW_ZERO: f64 = 0.0;

/// 语速过快侧的 0 分锚点（16 字/秒）。
///
/// 取甜区上限的两倍：16 已越过 [`crate::subtitle::stats`] 的中文「过快」线
///（12 CPS），这种语速观众必然跟不上，语速项直接归零。
const CPS_FAST_ZERO: f64 = 16.0;

/// 置信度量程下限（`avg_logprob`）：[-1.0, 0.0] 线性映射到 [0, 1]。
///
/// 实测分布（见 `segment.rs::DEFAULT_LOW_CONFIDENCE_THRESHOLD` 注释）集中在
/// [-0.67, -0.004]，取 [-1, 0] 作满量程正好让实测值铺开在中段：-1 ≈ 完全不可信、
/// 0 ≈ 完全可信，且不需要为「超出量程」写特判（下面统一 clamp）。
const CONFIDENCE_FLOOR: f64 = -1.0;

/// 置信度缺省分：0.5，**不奖不罚**。
///
/// SenseVoice / 旧记录 / 流式预览片段根本没有逐句置信度（`None`），这不代表
///「不可靠」，只代表「没有信号」。压分会冤枉整类引擎，抬分会凭空发糖，
/// 一律取中性值，且它恰好**不进** `reason` 摘要（见 [`REASON_MIN_SCORE`]）。
const CONFIDENCE_MISSING_SCORE: f64 = 0.5;

/// 章节加成：窗 start 距最近章节起点 ≤ 3 秒视为「就在章节开头」（满分 1.0）。
///
/// 3 秒约是一句话的长度：章节起点后还没讲完第一句话时，窗确实开在话题的开头。
/// 与衰减线（1/(1+3/30) ≈ 0.91）之间的跳变是刻意的——「开头」是质变而不是渐变。
const CHAPTER_NEAR_SECS: f64 = 3.0;

/// 章节加成的衰减半程（秒）：距离 30 秒处加成剩一半，90 秒外只剩约 1/4。
///
/// 30 秒约是「已经讲进细节里」的深度；衰减不归零是因为章节边界本身有误差
///（聚类是按目标时长切的，真实话题切换可能前后漂十几秒）。
const CHAPTER_DECAY_SECS: f64 = 30.0;

/// 章节信息缺省分：0.5，**不奖不罚**。
///
/// 调用方没传章节（或全被过滤成非法值）时没有任何结构信号可依据。取中性值而不是 0：
/// 缺信息不该全体压分，否则 `score` 失去「传不传章节都可比」的意义。
const CHAPTER_MISSING_SCORE: f64 = 0.5;

/// 时长甜区下缘（秒）：窗时长落在 [[LENGTH_FIT_FULL_LOW], [LENGTH_FIT_FULL_HIGH]]
/// 得满分 1.0。
///
/// 30~60 秒是短视频高光的完播甜区（抖音 / 视频号 / Shorts 的主流时长习惯，
/// OpusClip 的默认输出区间也在这附近）。
const LENGTH_FIT_FULL_LOW: f64 = 30.0;

/// 时长甜区上缘（秒）。
const LENGTH_FIT_FULL_HIGH: f64 = 60.0;

/// 时长贴合度在 [min_secs, max_secs] 边界处的分数（0.3）。
///
/// 不给 0：时长略偏不该全盘抹掉其它维度的贡献（一条 20 秒的高分内容仍然是高光）；
/// 钳在 0.3 表示「可选但不优」，让甜区窗在排序里稳赢。
const LENGTH_FIT_EDGE_SCORE: f64 = 0.3;

/// `reason` 摘要收录单项的得分下限：**严格高于中性 0.5** 才算「得分来源」。
///
/// 缺省项（无置信度 / 无章节）恰好落在 0.5，天然被排除——「不奖不罚」的维度
/// 不该被写成得分来源；得分不高的维度同理，理由栏只列真正抬分的项。
const REASON_MIN_SCORE: f64 = 0.5;

/// 兜底标题最大字数（按字符计，不是字节）。
///
/// 与 [`crate::utils::chapters`] 的兜底标题**同口径**（按字符截断、截断后去掉
/// 悬空标点），数值取 20：短视频封面/列表里的标题比章节标题更短，20 字与
/// 标题提示词要求模型给出的长度（「不超过 20 个字」）也保持一致。
const TITLE_MAX_CHARS: usize = 20;

/// `reason` 全部维度都不达标时的兜底文案。实际不可达（时长项恒 ≥ 0.3、
/// 章节项恒 > 0，但两者都可能不高于 [`REASON_MIN_SCORE`]），保留它是因为
/// 「reason 非空」是对外承诺，纯函数里不许出现空摘要的可能路径。
const REASON_FALLBACK: &str = "综合得分";

// ─────────────────────────────────────────────────────────────
// 主入口
// ─────────────────────────────────────────────────────────────

/// 规划高光切片候选：输入逐句字幕与参数，输出按时间升序的候选列表。
///
/// 边界防御（全部可单测，不 panic）：
/// - 空输入 / 全空文本 → 空表；
/// - 参数 NaN / 负数 / 退化区间 → 逐字段退回默认值；
/// - 入参乱序 → 处理内部排序副本，不改调用方数据，输出仍按 start 升序；
/// - 无解时**宁缺毋滥**：凑不满 `count` 就少给几条，绝不产出越界窗。
pub fn plan_highlight_clips(segments: &[Segment], opts: &ClipOptions) -> Vec<ClipCandidate> {
    let params = Params::sanitized(opts);
    let items = collect_items(segments);
    if items.is_empty() {
        return Vec::new();
    }

    // ①+② 每个锚句产出至多一个候选窗（越界 / 太短的窗直接丢弃）。
    let mut candidates: Vec<ClipCandidate> = Vec::new();
    for anchor in 0..items.len() {
        if let Some(candidate) = build_window_candidate(&items, anchor, &params) {
            candidates.push(candidate);
        }
    }

    // ③ 贪心去重：得分降序（同分按 start 升序——确定性），冲突者跳过。
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.start.total_cmp(&b.start))
            .then_with(|| a.end.total_cmp(&b.end))
    });
    let mut picked: Vec<ClipCandidate> = Vec::with_capacity(params.count.min(candidates.len()));
    'greedy: for candidate in candidates {
        if picked.len() >= params.count {
            break;
        }
        for taken in &picked {
            if conflicts(taken, &candidate, params.gap_secs) {
                continue 'greedy;
            }
        }
        picked.push(candidate);
    }

    // ④ 输出按时间升序：导出顺序即观看顺序。
    picked.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then_with(|| a.end.total_cmp(&b.end))
    });
    picked
}

// ─────────────────────────────────────────────────────────────
// 参数清洗
// ─────────────────────────────────────────────────────────────

/// 清洗后的参数。内部结构：调用方只面对 [`ClipOptions`]，清洗规则集中一处。
struct Params {
    count: usize,
    min_secs: f64,
    max_secs: f64,
    gap_secs: f64,
    chapter_starts: Vec<f64>,
}

impl Params {
    /// 逐字段退回默认值（对齐 [`crate::utils::chapters::plan_chapters`] 的防御风格）：
    /// - 时长：非有限或 ≤ 0 → 默认值；`min >= max`（区间退化 / 倒挂，任何窗都无法成立）
    ///   → 整对退回默认值，而不是各自为政让窗生成一半合法一半非法；
    /// - 间隔：非有限或负数 → 默认值（0 合法：不要求间隔）；
    /// - 章节：非有限 / 负数的条目直接丢弃（时间轴上没有负秒），其余原样保留。
    fn sanitized(opts: &ClipOptions) -> Self {
        let mut min_secs = if opts.min_secs.is_finite() && opts.min_secs > 0.0 {
            opts.min_secs
        } else {
            DEFAULT_MIN_SECS
        };
        let mut max_secs = if opts.max_secs.is_finite() && opts.max_secs > 0.0 {
            opts.max_secs
        } else {
            DEFAULT_MAX_SECS
        };
        if min_secs >= max_secs {
            min_secs = DEFAULT_MIN_SECS;
            max_secs = DEFAULT_MAX_SECS;
        }
        let gap_secs = if opts.gap_secs.is_finite() && opts.gap_secs >= 0.0 {
            opts.gap_secs
        } else {
            DEFAULT_GAP_SECS
        };
        let chapter_starts = opts
            .chapter_starts
            .iter()
            .copied()
            .filter(|s| s.is_finite() && *s >= 0.0)
            .collect();
        Self {
            count: opts.count,
            min_secs,
            max_secs,
            gap_secs,
            chapter_starts,
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 候选窗生成
// ─────────────────────────────────────────────────────────────

/// 参与切片的一句：非空文本、时间已清洗。
struct Item<'a> {
    start: f64,
    end: f64,
    /// `Segment::index`（不是数组下标）：定位要和 `AppState::select_segment` 对上。
    seg_index: usize,
    /// trim 后的 [`Segment::display_text`]（优先润色文本），恒非空。
    text: &'a str,
    confidence: Option<f64>,
}

/// 过滤 + 排序副本：空文本句不参与（它既不该锚定窗口、也不该贡献时长），
/// 时间非法的句跳过（`start` 非有限没法锚定；`end` 倒挂 / 缺失按零时长处理，
/// 保住文本但不贡献语音时长——与 chapters.rs 同口径）。
///
/// 排序键取 `(start, index, end, text)` 做**全序**：任何输入顺序都得到同一份
/// 窗序列，规划结果与调用方怎么排列无关（确定性测试逐字段锁死这一点）。
fn collect_items(segments: &[Segment]) -> Vec<Item<'_>> {
    let mut items: Vec<Item<'_>> = segments
        .iter()
        .filter_map(|seg| {
            if !seg.start.is_finite() {
                return None;
            }
            let text = seg.display_text().trim();
            if text.is_empty() {
                return None;
            }
            let start = seg.start.max(0.0);
            let end = if seg.end.is_finite() {
                seg.end.max(start)
            } else {
                start
            };
            Some(Item {
                start,
                end,
                seg_index: seg.index,
                text,
                confidence: seg.confidence,
            })
        })
        .collect();
    items.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then_with(|| a.seg_index.cmp(&b.seg_index))
            .then_with(|| a.end.total_cmp(&b.end))
            .then_with(|| a.text.cmp(b.text))
    });
    items
}

/// 以 `items[anchor]` 为锚生成一个候选窗并打分；窗不成立时返回 `None`。
fn build_window_candidate(
    items: &[Item<'_>],
    anchor: usize,
    params: &Params,
) -> Option<ClipCandidate> {
    let start = items[anchor].start;

    // 吞句：只要「再吞一句仍不越出 max_secs」就继续吞，得到 [min, max] 内的最长窗。
    // 到达 min 后**刻意不停**：内容更完整的窗优先（起因—结论都在窗里），过长的
    // 惩罚交给 length_fit 与 max 钳制。也正因吞句从不越出 max，「下一巨句被挡在
    // 窗外」时留下的单句短窗才会出现——那正是下面「单句 3 秒的窗不成立」要挡的。
    let mut last = anchor;
    let mut end = items[anchor].end;
    while last + 1 < items.len() {
        let next_end = end.max(items[last + 1].end);
        if next_end - start > params.max_secs {
            break;
        }
        last += 1;
        end = next_end;
    }

    // 唯一可能越出 max 的情形：锚点这一句自己就比 max 长（ASR 把整段讲稿粘成
    // 一条的「巨句」）。钳到 start + max 而不是丢弃：这种坏数据常常整段只有一条
    // segment，丢弃等于这段内容在候选列表里凭空消失；宁可交出一条被钳制的片段。
    let end = end.min(start + params.max_secs);
    let span = end - start;

    // 窗成立的两个条件（缺一不可）：
    // 1. 时长落在 [min_secs, max_secs]——切片长短是产品约束（测试逐条锁死），
    //    低于下限的窗宁可不产出（「无解时宁缺毋滥」），也不凑数；
    // 2. 「≥2 句」或「单句本身 ≥ min_secs」——单句 3 秒的窗撑不起一条高光。
    //    条件 2 在条件 1 下几乎恒真（单句窗的时长就是句长，钳制只会更短到 max ≥ min），
    //    显式写出来是把产品规则固化住，防止未来放宽时长约束时悄悄回归。
    let seg_count = last - anchor + 1;
    let single_span = items[anchor].end - items[anchor].start;
    let viable = seg_count >= 2 || single_span >= params.min_secs;
    if !viable || !(params.min_secs..=params.max_secs).contains(&span) {
        return None;
    }

    Some(score_window(&items[anchor..=last], start, end, params))
}

// ─────────────────────────────────────────────────────────────
// 打分
// ─────────────────────────────────────────────────────────────

/// 单项得分来源：贡献（权重×得分）、得分本身、中文标签。`reason` 的原料。
struct ScoreSource {
    contribution: f64,
    score: f64,
    label: String,
}

/// 对一个成立的窗打分，产出候选。
fn score_window(window: &[Item<'_>], start: f64, end: f64, params: &Params) -> ClipCandidate {
    let mut excited = 0usize;
    let mut chars = 0usize;
    let mut speech_secs = 0.0;
    let mut confidence_sum = 0.0;
    for item in window {
        if is_excitement_ending(item.text) {
            excited += 1;
        }
        // 字符口径与 stats.rs 完全一致（visible_char_count：chars() 去空白）：
        // 阅读速度的「字数」必须和统计面板同一个定义，否则两处 CPS 对不上。
        chars += visible_char_count(item.text);
        // 有效语音时长 = 各句时长之和（段间空白不计）：CPS 分母只该算「在说话」的时间。
        speech_secs += (item.end - item.start).max(0.0);
        confidence_sum += confidence_score(item.confidence);
    }
    let n = window.len() as f64;

    let engagement = excited as f64 / n;
    // 有效语音为 0（全是零长句）时没有「语速」可言：给 0 分而不是除出 inf/NaN——
    // 一个 NaN 会污染加权和，让整条候选的 score 变成 NaN，排序随之失效。
    let speech_fit = if speech_secs > 0.0 {
        speech_fit_score(chars as f64 / speech_secs)
    } else {
        0.0
    };
    let confidence = confidence_sum / n;
    let (chapter_bonus, chapter_dist) = chapter_bonus(start, &params.chapter_starts);
    let span = end - start;
    let length_fit = length_fit_score(span, params.min_secs, params.max_secs);

    // 加权求和后除以权重总和归一化：权重表将来若被调过，score 仍恒在 [0,1]。
    let total_weight = WEIGHT_ENGAGEMENT
        + WEIGHT_SPEECH_FIT
        + WEIGHT_CONFIDENCE
        + WEIGHT_CHAPTER_BONUS
        + WEIGHT_LENGTH_FIT;
    let weighted = WEIGHT_ENGAGEMENT * engagement
        + WEIGHT_SPEECH_FIT * speech_fit
        + WEIGHT_CONFIDENCE * confidence
        + WEIGHT_CHAPTER_BONUS * chapter_bonus
        + WEIGHT_LENGTH_FIT * length_fit;
    let score = (weighted / total_weight).clamp(0.0, 1.0);

    let reason = build_reason(&[
        ScoreSource {
            contribution: WEIGHT_ENGAGEMENT * engagement,
            score: engagement,
            label: format!("疑问句×{excited}"),
        },
        ScoreSource {
            contribution: WEIGHT_SPEECH_FIT * speech_fit,
            score: speech_fit,
            label: "语速适中".to_string(),
        },
        ScoreSource {
            contribution: WEIGHT_CONFIDENCE * confidence,
            score: confidence,
            label: "置信度高".to_string(),
        },
        ScoreSource {
            contribution: WEIGHT_CHAPTER_BONUS * chapter_bonus,
            score: chapter_bonus,
            label: chapter_label(chapter_dist),
        },
        ScoreSource {
            contribution: WEIGHT_LENGTH_FIT * length_fit,
            score: length_fit,
            label: "时长合适".to_string(),
        },
    ]);

    let first = &window[0];
    ClipCandidate {
        start,
        end,
        score,
        reason,
        seg_indices: window.iter().map(|item| item.seg_index).collect(),
        title: truncate_chars(first_sentence(first.text), TITLE_MAX_CHARS),
    }
}

/// 句子是否以「情绪 / 悬念」收尾（[`EXCITEMENT_ENDINGS`] 之一）。
fn is_excitement_ending(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|c| EXCITEMENT_ENDINGS.contains(&c))
}

/// 单句置信度归一化：`avg_logprob` 的 [-1.0, 0.0] 线性映射到 [0, 1]（越小越不可靠，
/// 所以 -1 → 0、0 → 1），量程外 clamp；`None` 见 [`CONFIDENCE_MISSING_SCORE`]。
fn confidence_score(confidence: Option<f64>) -> f64 {
    match confidence {
        Some(c) => ((c - CONFIDENCE_FLOOR) / (0.0 - CONFIDENCE_FLOOR)).clamp(0.0, 1.0),
        None => CONFIDENCE_MISSING_SCORE,
    }
}

/// 语速贴近度：甜区 [[CPS_SWEET_LOW], [CPS_SWEET_HIGH]] 内满分，
/// 两侧线性衰减，慢侧到 [`CPS_SLOW_ZERO`] 归零、快侧到 [`CPS_FAST_ZERO`] 归零。
fn speech_fit_score(cps: f64) -> f64 {
    if cps < CPS_SWEET_LOW {
        ((cps - CPS_SLOW_ZERO) / (CPS_SWEET_LOW - CPS_SLOW_ZERO)).clamp(0.0, 1.0)
    } else if cps > CPS_SWEET_HIGH {
        ((CPS_FAST_ZERO - cps) / (CPS_FAST_ZERO - CPS_SWEET_HIGH)).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// 章节加成：窗 start 距最近章节起点 ≤ [`CHAPTER_NEAR_SECS`] → 1.0，
/// 否则按 `1/(1+距离/[CHAPTER_DECAY_SECS])` 衰减；无章节信息 → [`CHAPTER_MISSING_SCORE`]。
/// 第二个返回值是最近距离（`None` 表示无章节），供 `reason` 标签区分「开头 / 附近」。
fn chapter_bonus(window_start: f64, chapter_starts: &[f64]) -> (f64, Option<f64>) {
    let nearest = chapter_starts
        .iter()
        .map(|c| (window_start - *c).abs())
        .min_by(|a, b| a.total_cmp(b));
    let Some(dist) = nearest else {
        return (CHAPTER_MISSING_SCORE, None);
    };
    if dist <= CHAPTER_NEAR_SECS {
        (1.0, Some(dist))
    } else {
        (1.0 / (1.0 + dist / CHAPTER_DECAY_SECS), Some(dist))
    }
}

/// 章节维度的 `reason` 标签。
fn chapter_label(dist: Option<f64>) -> String {
    match dist {
        Some(d) if d <= CHAPTER_NEAR_SECS => "章节开头".to_string(),
        Some(_) => "章节附近".to_string(),
        None => "章节信息缺省".to_string(),
    }
}

/// 时长贴合度：甜区 [[LENGTH_FIT_FULL_LOW], [LENGTH_FIT_FULL_HIGH]] 内满分，
/// 两侧线性衰减到 [min_secs, max_secs] 边界处的 [`LENGTH_FIT_EDGE_SCORE`]。
fn length_fit_score(span: f64, min_secs: f64, max_secs: f64) -> f64 {
    if (LENGTH_FIT_FULL_LOW..=LENGTH_FIT_FULL_HIGH).contains(&span) {
        return 1.0;
    }
    if span < LENGTH_FIT_FULL_LOW {
        // 下边界侧：min_secs 处 0.3 线性升到 30s 处 1.0。窗成立前置条件保证
        // span >= min_secs；分母兜底 ε 只防 min_secs 被抬到 30 以上后除零
        //（此时该分支本就不可达，属纯防御）。
        let denom = (LENGTH_FIT_FULL_LOW - min_secs).max(f64::EPSILON);
        (LENGTH_FIT_EDGE_SCORE + (1.0 - LENGTH_FIT_EDGE_SCORE) * (span - min_secs) / denom)
            .clamp(LENGTH_FIT_EDGE_SCORE, 1.0)
    } else {
        // 上边界侧：60s 处 1.0 线性降到 max_secs 处 0.3。
        let denom = (max_secs - LENGTH_FIT_FULL_HIGH).max(f64::EPSILON);
        (1.0 - (1.0 - LENGTH_FIT_EDGE_SCORE) * (span - LENGTH_FIT_FULL_HIGH) / denom)
            .clamp(LENGTH_FIT_EDGE_SCORE, 1.0)
    }
}

/// 汇总 `reason`：只收**高于中性**（[`REASON_MIN_SCORE`]）的维度，按贡献降序取前两项，
/// 「、」连接（如「疑问句×2、章节开头」）。并列时按传入顺序（固定组件顺序），
/// 稳定排序保证两次运行文案逐字一致。
fn build_reason(sources: &[ScoreSource]) -> String {
    let mut positive: Vec<&ScoreSource> = sources
        .iter()
        .filter(|s| s.score > REASON_MIN_SCORE)
        .collect();
    positive.sort_by(|a, b| b.contribution.total_cmp(&a.contribution));
    if positive.is_empty() {
        return REASON_FALLBACK.to_string();
    }
    positive
        .iter()
        .take(2)
        .map(|s| s.label.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

// ─────────────────────────────────────────────────────────────
// 去重
// ─────────────────────────────────────────────────────────────

/// 两条候选是否冲突：区间相交（首尾相接不算相交），或时间距离 < `gap_secs`。
fn conflicts(a: &ClipCandidate, b: &ClipCandidate, gap_secs: f64) -> bool {
    if a.start < b.end && b.start < a.end {
        return true;
    }
    let dist = if a.end <= b.start {
        b.start - a.end
    } else {
        a.start - b.end
    };
    dist < gap_secs
}

// ─────────────────────────────────────────────────────────────
// 标题兜底（口径对齐 utils/chapters.rs）
// ─────────────────────────────────────────────────────────────

/// 取第一句：在句末标点处切开，返回第一段非空白内容。
///
/// 与 [`crate::utils::chapters`] 同口径：`'.'` 只在后面紧跟空白或行尾时才算句末，
/// 否则「3.5 小时」会被切成「3」。chapters.rs 的实现是私有工具（本模块刻意不改它），
/// 这里按同一规则重写，两处兜底标题才不会一个切在小数点、一个不切。
fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    let mut sentence_start = 0usize;
    for (i, ch) in text.char_indices() {
        let is_end = match ch {
            '。' | '！' | '？' | '!' | '?' | '；' | ';' | '…' | '\n' | '\r' => true,
            '.' => match text[i + 1..].chars().next() {
                None => true,
                Some(next) => next.is_whitespace(),
            },
            _ => false,
        };
        if is_end {
            let candidate = text[sentence_start..i].trim();
            if !candidate.is_empty() {
                return candidate;
            }
            sentence_start = i + ch.len_utf8();
        }
    }
    text[sentence_start..].trim()
}

/// 按字符（不是字节）截断，截断后去掉悬空的标点。
///
/// 中文字符是 3 字节，按字节截断会把标题劈成非法 UTF-8 的乱码；截断落在
/// 「，」「、」上时看起来像没写完，顺手抹掉。口径与 chapters.rs 一致。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        while out.chars().next_back().is_some_and(is_trailing_separator) {
            out.pop();
        }
    }
    out.trim_end().to_string()
}

fn is_trailing_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '，' | ',' | '、' | '；' | ';' | '：' | ':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(index: usize, start: f64, end: f64, text: &str) -> Segment {
        Segment::new(index, start, end, text)
    }

    /// 浮点断言统一走容差，避开 `clippy::float_cmp`。
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// 构造「一个窗只含两句、总长 10 秒」的簇：锚在首句得 [cs, cs+10]，锚在次句
    /// 只剩 5 秒（< min 8）不成立——每簇恰好贡献一个候选，对照实验才干净。
    fn cluster(
        base: f64,
        first: &str,
        second: &str,
        confidences: [Option<f64>; 2],
    ) -> Vec<Segment> {
        let mut a = seg(0, base, base + 5.0, first);
        a.confidence = confidences[0];
        let mut b = seg(0, base + 5.0, base + 10.0, second);
        b.confidence = confidences[1];
        vec![a, b]
    }

    /// 对照实验共用参数：min 8 / max 12 让每簇只出一个窗。
    fn compare_opts() -> ClipOptions {
        ClipOptions {
            min_secs: 8.0,
            max_secs: 12.0,
            ..ClipOptions::default()
        }
    }

    /// 确定性：同输入跑两次，逐字段相等（含 reason / title 文案）。
    #[test]
    fn planning_is_deterministic() {
        let mut segs = cluster(0.0, "这是什么呢？", "为什么呢？", [Some(-0.2), None]);
        segs.extend(cluster(200.0, "今天天气。", "挺好的。", [None, Some(-0.6)]));
        segs.extend(cluster(
            400.0,
            "结果没想到……",
            "真的吗？！",
            [Some(0.0), Some(-1.0)],
        ));
        let opts = ClipOptions {
            chapter_starts: vec![0.0, 400.0],
            ..compare_opts()
        };

        let first = plan_highlight_clips(&segs, &opts);
        let second = plan_highlight_clips(&segs, &opts);
        assert_eq!(first, second, "两次规划必须逐字段一致: {first:?}");
        assert!(!first.is_empty());
    }

    /// 时长约束：每条候选都落在 [min, max]；且能取满 count 时按时间升序取满。
    #[test]
    fn candidates_respect_duration_bounds_and_count() {
        // 8 个内容完全相同的簇、间隔 100s（> max 90），每簇恰出一个 20s 窗。
        let mut segs = Vec::new();
        for c in 0..8usize {
            let base = c as f64 * 100.0;
            let mut first = seg(0, base, base + 10.0, "今天我们开始。");
            first.index = c * 2 + 1;
            let mut second = seg(0, base + 10.0, base + 20.0, "先看第一个问题。");
            second.index = c * 2 + 2;
            segs.push(first);
            segs.push(second);
        }

        let out = plan_highlight_clips(&segs, &ClipOptions::default());
        assert_eq!(out.len(), 5, "候选充足时取满 count: {out:?}");
        assert!(
            out.iter()
                .all(|c| (DEFAULT_MIN_SECS..=DEFAULT_MAX_SECS).contains(&(c.end - c.start))),
            "每条都必须在 [min, max] 内: {out:?}"
        );
        assert!(
            out.windows(2).all(|w| w[0].start < w[1].start),
            "输出按 start 升序: {out:?}"
        );
        // 同分候选按 start 升序贪心，因此取到的是最早的 5 簇。
        let starts: Vec<f64> = out.iter().map(|c| c.start).collect();
        assert_eq!(starts, vec![0.0, 100.0, 200.0, 300.0, 400.0], "{out:?}");

        let capped = plan_highlight_clips(
            &segs,
            &ClipOptions {
                count: 2,
                ..ClipOptions::default()
            },
        );
        assert_eq!(capped.len(), 2, "count 是硬上限: {capped:?}");
    }

    /// 无解时宁缺毋滥：凑不出 [min, max] 窗的输入直接给空表，绝不产出越界窗。
    #[test]
    fn no_valid_window_yields_fewer_not_worse() {
        // 总长 10s 的连续短句：任何窗都到不了 min 15s。
        let segs: Vec<Segment> = (0..5)
            .map(|i| seg(i + 1, i as f64 * 2.0, i as f64 * 2.0 + 2.0, "短句。"))
            .collect();
        assert!(
            plan_highlight_clips(&segs, &ClipOptions::default()).is_empty(),
            "宁可一条不给，也不给不合格的窗"
        );

        // 单句 3 秒：「单句 3 秒的窗不成立」。
        assert!(
            plan_highlight_clips(&[seg(1, 0.0, 3.0, "短句。")], &ClipOptions::default()).is_empty()
        );
    }

    /// gap 去重：时间距离 < gap_secs 的两条候选只留得分高的那条（提问句胜出）。
    #[test]
    fn gap_dedup_keeps_only_the_stronger_neighbour() {
        // [0,8] 提问窗 vs [11,19] 陈述窗：间隔 3s < gap 5s，必须二选一。
        let segs = vec![
            seg(1, 0.0, 4.0, "你怎么看？"),
            seg(2, 4.0, 8.0, "真的吗？"),
            seg(3, 11.0, 15.0, "今天天气。"),
            seg(4, 15.0, 19.0, "挺好的。"),
        ];
        let opts = ClipOptions {
            min_secs: 5.0,
            max_secs: 10.0,
            ..ClipOptions::default()
        };
        let out = plan_highlight_clips(&segs, &opts);
        assert_eq!(out.len(), 1, "间隔不足 gap 的候选互相排斥: {out:?}");
        assert!(close(out[0].start, 0.0), "留下得分更高的提问窗: {out:?}");
        assert!(close(out[0].end, 8.0));
        assert_eq!(out[0].seg_indices, vec![1, 2]);
        assert!(out[0].reason.contains("疑问句"), "{}", out[0].reason);
    }

    /// 章节加成为正向：内容完全相同的两个窗，靠近章节起点的得分严格更高，
    /// 且 reason 提到章节。
    #[test]
    fn chapter_bonus_lifts_nearby_window() {
        let mut segs = cluster(0.0, "这是什么呢？", "为什么呢？", [None, None]);
        segs.extend(cluster(200.0, "这是什么呢？", "为什么呢？", [None, None]));
        let opts = ClipOptions {
            chapter_starts: vec![0.0],
            ..compare_opts()
        };

        let out = plan_highlight_clips(&segs, &opts);
        assert_eq!(out.len(), 2, "{out:?}");
        let near = out.iter().find(|c| close(c.start, 0.0)).expect("近窗");
        let far = out.iter().find(|c| close(c.start, 200.0)).expect("远窗");
        assert!(
            near.score > far.score,
            "章节加成必须严格抬分: near={} far={}",
            near.score,
            far.score
        );
        assert!(near.reason.contains("章节"), "{}", near.reason);
    }

    /// 疑问句确实抬分：对照组只差收尾标点（？ vs 。），得分必须拉开。
    #[test]
    fn question_sentences_raise_score() {
        let mut segs = cluster(0.0, "这是什么呢？", "为什么呢？", [None, None]);
        segs.extend(cluster(200.0, "这是什么呢。", "为什么呢。", [None, None]));
        let out = plan_highlight_clips(&segs, &compare_opts());
        assert_eq!(out.len(), 2, "{out:?}");
        let questioning = out.iter().find(|c| close(c.start, 0.0)).expect("提问窗");
        let plain = out.iter().find(|c| close(c.start, 200.0)).expect("陈述窗");
        assert!(
            questioning.score > plain.score,
            "疑问句必须抬分: q={} p={}",
            questioning.score,
            plain.score
        );
        assert!(
            questioning.reason.contains("疑问句"),
            "{}",
            questioning.reason
        );
    }

    /// confidence 缺省不奖不罚：全缺省与全取量程中点（-0.5）得分一致；
    /// 全满分更高、全最低更低。
    #[test]
    fn missing_confidence_is_neutral() {
        let mut segs = cluster(0.0, "这是什么呢。", "为什么呢。", [None, None]);
        segs.extend(cluster(
            200.0,
            "这是什么呢。",
            "为什么呢。",
            [Some(-0.5), Some(-0.5)],
        ));
        segs.extend(cluster(
            400.0,
            "这是什么呢。",
            "为什么呢。",
            [Some(0.0), Some(0.0)],
        ));
        segs.extend(cluster(
            600.0,
            "这是什么呢。",
            "为什么呢。",
            [Some(-1.0), Some(-1.0)],
        ));
        let out = plan_highlight_clips(&segs, &compare_opts());
        assert_eq!(out.len(), 4, "{out:?}");

        let score_at = |start: f64| {
            out.iter()
                .find(|c| close(c.start, start))
                .unwrap_or_else(|| panic!("缺少 {start}s 处的候选: {out:?}"))
                .score
        };
        let missing = score_at(0.0);
        let midpoint = score_at(200.0);
        let best = score_at(400.0);
        let worst = score_at(600.0);
        assert!(
            close(missing, midpoint),
            "缺省必须与中点同权: {missing} vs {midpoint}"
        );
        assert!(best > missing, "满分置信度应抬分");
        assert!(missing > worst, "最低置信度应压分");
    }

    /// 乱序输入不 panic、不改调用方数据，输出与有序输入逐字段一致且按 start 升序。
    #[test]
    fn unordered_input_is_safe_and_sorted() {
        let mut segs = cluster(0.0, "这是什么呢？", "为什么呢？", [Some(-0.3), None]);
        segs.extend(cluster(200.0, "今天天气。", "挺好的。", [None, None]));
        segs.extend(cluster(
            400.0,
            "结果没想到……",
            "真的吗？！",
            [Some(0.0), Some(-0.8)],
        ));
        for (i, s) in segs.iter_mut().enumerate() {
            s.index = i + 1;
        }
        let mut shuffled = segs.clone();
        shuffled.reverse();

        let opts = ClipOptions {
            chapter_starts: vec![0.0, 400.0],
            ..compare_opts()
        };
        let out = plan_highlight_clips(&shuffled, &opts);
        assert_eq!(
            out,
            plan_highlight_clips(&segs, &opts),
            "入参顺序不得影响结果"
        );
        assert!(out.windows(2).all(|w| w[0].start < w[1].start), "{out:?}");
    }

    /// 空输入 / 全空文本 / count=0 → 空表。
    #[test]
    fn empty_input_yields_empty() {
        assert!(plan_highlight_clips(&[], &ClipOptions::default()).is_empty());
        let blanks = vec![seg(1, 0.0, 5.0, "   "), seg(2, 5.0, 10.0, "")];
        assert!(
            plan_highlight_clips(&blanks, &ClipOptions::default()).is_empty(),
            "全空文本没有可切的内容"
        );
        let segs = cluster(0.0, "这是什么呢？", "为什么呢？", [None, None]);
        assert!(
            plan_highlight_clips(
                &segs,
                &ClipOptions {
                    count: 0,
                    ..compare_opts()
                }
            )
            .is_empty(),
            "count=0 的语义是「一条都不产出」"
        );
    }

    /// 单句超长窗被 max 钳制：巨句产出恰好 max_secs 长的候选，而不是超长切片、
    /// 也不是整段消失；被巨句挡住去路的单句短窗不成立。
    #[test]
    fn overlong_single_window_is_clamped_by_max() {
        let giant = seg(1, 0.0, 200.0, "这是一句被 ASR 粘成一团的超长巨句。");
        let out = plan_highlight_clips(&[giant], &ClipOptions::default());
        assert_eq!(out.len(), 1, "巨句不该凭空消失: {out:?}");
        assert!(close(out[0].start, 0.0));
        assert!(
            close(out[0].end, DEFAULT_MAX_SECS),
            "终点钳到 start+max: {out:?}"
        );
        assert!(close(out[0].end - out[0].start, DEFAULT_MAX_SECS));
        assert_eq!(out[0].seg_indices, vec![1]);

        // 短句在前、巨句在后：3 秒短句被巨句挡在窗外（单句短窗不成立），
        // 巨句自己的锚仍产出钳制窗。
        let segs = vec![
            seg(1, 0.0, 3.0, "短句。"),
            seg(2, 3.0, 203.0, "巨句覆盖了整整两百秒的内容。"),
        ];
        let out = plan_highlight_clips(&segs, &ClipOptions::default());
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(close(out[0].start, 3.0), "{out:?}");
        assert!(close(out[0].end, 3.0 + DEFAULT_MAX_SECS), "{out:?}");
    }

    /// reason 恒非空；title 取首句、按字符截 20 字（口径同 chapters.rs）。
    #[test]
    fn reason_and_title_are_usable() {
        let long_first = "今天我们来非常详细地聊一聊如何在一个完全陌生的项目里快速定位性能瓶颈并把它修好。第二句不该进标题。";
        let out = plan_highlight_clips(&[seg(1, 0.0, 30.0, long_first)], &ClipOptions::default());
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(!out[0].reason.trim().is_empty(), "reason 恒非空");
        assert_eq!(out[0].title, "今天我们来非常详细地聊一聊如何在一个完全");
        assert_eq!(out[0].title.chars().count(), TITLE_MAX_CHARS);
        assert!(!out[0].title.contains("第二句"));

        // 各类候选的 reason / title 都不能是空串。
        let mut segs = cluster(0.0, "这是什么呢。", "为什么呢。", [None, None]);
        segs.extend(cluster(
            200.0,
            "结果没想到……",
            "真的吗？！",
            [Some(-0.1), Some(-0.2)],
        ));
        for c in plan_highlight_clips(&segs, &compare_opts()) {
            assert!(!c.reason.trim().is_empty(), "{c:?}");
            assert!(!c.title.trim().is_empty(), "{c:?}");
            assert!(c.title.chars().count() <= TITLE_MAX_CHARS, "{}", c.title);
        }
    }

    /// 非法参数（NaN / 负数 / 退化区间）逐字段退回默认值，行为与默认参数完全一致。
    #[test]
    fn invalid_params_fall_back_to_defaults() {
        // 两簇各 20s、间隔 200s：默认参数（min 15 / max 90）下每簇恰出一个窗。
        let segs = vec![
            seg(1, 0.0, 10.0, "这是什么呢？"),
            seg(2, 10.0, 20.0, "为什么呢？"),
            seg(3, 200.0, 210.0, "今天天气。"),
            seg(4, 210.0, 220.0, "挺好的。"),
        ];

        let baseline = plan_highlight_clips(&segs, &ClipOptions::default());
        assert!(!baseline.is_empty());

        let dirty = ClipOptions {
            min_secs: f64::NAN,
            max_secs: -5.0,
            gap_secs: f64::NAN,
            chapter_starts: vec![f64::NAN, -3.0],
            ..ClipOptions::default()
        };
        assert_eq!(
            plan_highlight_clips(&segs, &dirty),
            baseline,
            "NaN/负数参数必须退回默认值"
        );

        let degenerate = ClipOptions {
            min_secs: 90.0,
            max_secs: 15.0,
            ..ClipOptions::default()
        };
        assert_eq!(
            plan_highlight_clips(&segs, &degenerate),
            baseline,
            "min >= max 的退化区间整对退回默认值"
        );
    }
}
