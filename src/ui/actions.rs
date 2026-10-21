//! MainWindow 异步与后台业务动作逻辑

use gpui::prelude::*;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tracing::info;

use super::types::{BatchSummary, CompletionDialogInfo};
use super::{
    apply_replace_rule_set, apply_replace_rule_set_to_indices, find_replace_rule_set,
    remove_replace_rule_set, replace_rule_sets_from_json, replace_rule_sets_to_json,
    save_replace_rule_sets_default, upsert_replace_rule_set, EditorExportFormat, MainWindow,
    ReplaceRule, ReplaceRuleReport, ReplaceRuleScope, ReplaceRuleSet, REPLACE_RULES_FILE,
};
use crate::app::state::{AppState, ProcessStatus, QueueState, WorkspaceTab};
use crate::core::PipelineEvent;
use crate::subtitle::audit::{EditKind, EditRecord};
use crate::subtitle::writer::export_spec_for;
use crate::subtitle::Segment;
use crate::subtitle::SubtitleWriter;
use crate::subtitle::{CpsFixPlan, DEFAULT_MAX_CPS, MIN_EDIT_DUR};

/// 导入字幕成功后的提示文案（**纯函数**，便于单测）。
///
/// 返回字符串而不是直接写 `this.notice`：混合拖入时调用方要在前面拼上「媒体已
/// 入队」的摘要，拿到文案才谈得上组合；纯函数也让三条分支能脱开 `MainWindow`
/// 单测。
///
/// TXT 分支必须显式提醒时间轴是合成的——用户很可能接着导出 SRT，而那份时间
/// 完全不是真的。
///
/// LRC 同样要提醒，但理由和措辞都不同：LRC 每行只带**起点**时间，`reader` 用
/// 「下一行的起点」补齐中间各句的终点，只有**最后一句**的终点是按字数估算的。
/// 也就是说除末句外的时间轴都来自文件里的真实标注，唯独末句是猜的，所以文案
/// 只把末句点出来，不必像 TXT 那样否定整条时间轴。
fn import_notice(stem: &str, count: usize, is_txt: bool, is_lrc: bool) -> String {
    if is_txt {
        format!(
            "已导入「{stem}」{count} 句；纯文本没有时间信息，已按字数合成占位时间轴，导出前请核对"
        )
    } else if is_lrc {
        format!("已导入「{stem}」{count} 句；最后一句的结束时间为按字数估算，导出前请核对")
    } else {
        format!("已导入「{stem}」{count} 句字幕")
    }
}

/// 术语表导入结果的提示文案（**纯函数**，便于单测）。
///
/// 三条分支：文件里一个条目都没解析出来（空提示，告诉用户检查格式）、
/// 条目全都已存在（新增 0 / 覆盖 0）、以及有增有改的正常情况。
fn glossary_import_notice(added: usize, overwritten: usize, unchanged: usize) -> String {
    if added == 0 && overwritten == 0 {
        if unchanged == 0 {
            "导入的术语表里没有条目，请检查文件格式（每行「原文=译文」）".to_string()
        } else {
            format!("术语表已是最新：未变 {unchanged} 条，没有新增或覆盖")
        }
    } else {
        format!("已导入术语表：新增 {added} / 覆盖 {overwritten} / 未变 {unchanged}")
    }
}

/// 阅读速度一键修正的结果提示文案（**纯函数**，便于单测）。
///
/// 四条分支：整体无需改动、只延长成功、延长了一部分且仍有延不了的、以及
/// 一句都没延长（`extended` 为空但 `unresolved` 非空）——最后一种不能当成
/// 「都在合理范围内」蒙混过去，否则用户看到「没问题」却仍有一屏读不完的字幕。
fn cps_fix_notice(plan: &CpsFixPlan) -> String {
    match (plan.extended.len(), plan.unresolved.len()) {
        (0, 0) => "语速都在合理范围内".to_string(),
        (0, n) => format!("有 {n} 句时长不够、但后一句贴得太近延不开；请压后一句或把这句改短"),
        (e, 0) => format!("已延长 {e} 句，阅读速度回到合理范围"),
        (e, n) => format!("已延长 {e} 句，另有 {n} 句因后一句贴得太近未能延到位，请手动调整"),
    }
}

/// `EditorExportFormat`（剪辑台的状态枚举）与配置字符串（`state.output_format`）
/// 之间的唯一桥梁：返回 `writer::write_in_place` / `export_spec_for` 认的规范格式名。
///
/// 之所以只映射到**格式字符串**、扩展名与过滤器名交给
/// [`export_spec_for`] 再算一次：这几项字面量原本在 `perform_editor_export`
/// 里被内联重复了一遍，两份表一旦漂移就会出现「文件叫 `.ttml`、过滤器却写
/// 别的格式」。`src/ui/mod.rs` 的枚举定义不动，此处只做「枚举 -> 字符串」，
/// 扩展名/过滤器名仍只有 `writer.rs` 一个来源。
///
/// 剪映（含导出到文件夹）/ FCPXML / Premiere XML 不走这条单文件链路
/// （整个草稿目录或专用 exporter），由 `perform_editor_export` 的 `match`
/// 提前分流，不会走到这里。
/// 烧录临时 ASS 的进程内唯一序号（跨进程由 pid 区分，进程内靠原子自增）。
///
/// 与 `ffmpeg::temp_wav_path` 同一思路：避免并发/连续烧录时两条任务算出同一个
/// `%TEMP%` 文件名，后写者覆盖前一个仍在被 FFmpeg 读取的字幕。
fn burn_temp_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 「按句导出片段」是否正在进行。
///
/// 用模块内的原子量而不是 `MainWindow` 字段：既有的忙标志（代理转码 / 波形 /
/// 更新下载）都各自绑定在别的子系统的语义上，借来用会把「正在导出片段」误报成
/// 那些状态。这个自包含的标志只做两件事——挡住重复点击、让入口按钮在导出期间
/// 置灰，不触碰既有状态机。
pub(crate) static CLIP_EXPORT_BUSY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn editor_export_format_string(fmt: EditorExportFormat) -> &'static str {
    match fmt {
        EditorExportFormat::Srt => "srt",
        EditorExportFormat::Ass => "ass",
        EditorExportFormat::Txt => "txt",
        EditorExportFormat::Vtt => "vtt",
        EditorExportFormat::Json => "json",
        EditorExportFormat::EbuTtD => "ttml",
        EditorExportFormat::NetflixTtal => "ttal",
        EditorExportFormat::Sbv => "sbv",
        EditorExportFormat::Lrc => "lrc",
        // 注意返回的是 **writer 认的格式名** 而不是扩展名：`txt-ts` 与 `txt` 同写
        // `.txt`，内容却不同（每行带时间码 vs 纯文本）。调用方写盘要用这个格式名，
        // 扩展名另由 `export_spec_for` 给出（见 `perform_editor_export`）。
        EditorExportFormat::TxtTs => "txt-ts",
        // 上面这几个是唯一会走到单文件导出的分支；其余档位在上层已分流。
        // 真被漏到这里时给最保守的 srt（writer 一定写得出来的格式），
        // 也比 panic 掉整个 UI 线程好。
        _ => "srt",
    }
}

/// 支持导入的音视频扩展名。文件对话框、拖放、批量队列三处共用同一份，
/// 避免某个入口悄悄漏掉一种格式。
pub(crate) const MEDIA_EXTS: [&str; 10] = [
    "mp4", "mkv", "mov", "avi", "flv", "webm", "mp3", "wav", "flac", "m4a",
];

/// 支持**拖拽导入**的字幕扩展名（P1-A11）。
///
/// 刻意与拖拽的媒体清单一样由 UI 层集中定义：拖放判定是「按扩展名自动路由」，
/// 清单必须和 [`MainWindow::handle_dropped_files`] 的提示文案一一对应，散在
/// 两处迟早对不上。
///
/// 它比「导入字幕」对话框的过滤器少了 `txt`：`.txt` 在拖放语境里意图太不确定，
/// 随手拖进来的备忘、脚本、日志都会被当成字幕并**直接顶掉当前工程**；而在对话框
/// 里点选文件名时用户意图明确，那边才保留 txt。
pub(crate) const SUBTITLE_EXTS: [&str; 10] = [
    "srt", "vtt", "json", "lrc", "ass", "ssa", "ttml", "dfxp", "ttal", "sbv",
];

/// 「导入字幕」对话框的文件过滤器：比拖拽清单多一个 `txt`——点选文件时用户意图明确，
/// 纯文本（时间轴由导入侧按字数合成）也允许。除 `txt` 外都取自
/// `subtitle::reader::SubtitleFormat::from_path` 认识的扩展名，两份清单必须同源。
#[allow(dead_code)]
pub(crate) const IMPORT_SUBTITLE_EXTS: [&str; 11] = [
    "srt", "vtt", "txt", "json", "lrc", "ass", "ssa", "ttml", "dfxp", "ttal", "sbv",
];

/// 一次拖拽进来的内容该怎么走。
///
/// 决策只由两个**计数**决定（已有清单长度），因此可以抽成纯函数单测；真正改状态
/// 的那几段分别要入队、探测时长、spawn 后台读盘，没法在单测里跑。把决策和执行
/// 分开，「几号媒体、几号字幕走哪条路」这条规则才能被测试钉死。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropRoute {
    /// 一个媒体一个字幕都没认出来
    Nothing,
    /// 只有媒体：沿用既有的单文件选中 / 多文件入队
    MediaOnly,
    /// 只有字幕且正好一个：直接导入
    SubtitleOnly,
    /// 只有字幕但有多个：明确提示，不猜、不静默
    TooManySubtitles,
    /// 媒体 + 一个字幕：媒体入队，同时导入字幕
    MediaAndSubtitle,
    /// 媒体 + 多个字幕：媒体入队，字幕只提示
    MediaAndTooManySubtitles,
}

/// 拖拽路由决策：`media` / `subtitle` 是**已展开**的清单长度（媒体含目录递归的
/// 结果，字幕只含用户点名的文件，见 `media_scan::collect_subtitle_files`）。
///
/// 规则：媒体怎么处理不受字幕影响（进队列的照样进队列），字幕则是「一个才导入、
/// 多了只提示」——一次导入多个字幕没有合理的合并语义（时间轴互相覆盖），猜一个
/// 等于替用户做破坏性选择。
fn drop_route(media: usize, subtitle: usize) -> DropRoute {
    match (media, subtitle) {
        (0, 0) => DropRoute::Nothing,
        (0, 1) => DropRoute::SubtitleOnly,
        (0, _) => DropRoute::TooManySubtitles,
        (_, 0) => DropRoute::MediaOnly,
        (_, 1) => DropRoute::MediaAndSubtitle,
        (_, _) => DropRoute::MediaAndTooManySubtitles,
    }
}

/// 一次拖入多个字幕文件时的提示（纯函数，便于单测）。
///
/// 措辞必须给出**下一步动作**：用户拖了 3 份字幕，只说「不支持」等于让他自己猜
/// 该怎么办；说清「一次只导入一个、请分开拖入」，他立刻知道怎么继续。
fn too_many_subtitles_notice(count: usize) -> String {
    format!("检测到 {count} 个字幕文件；一次只导入一个字幕文件，请分开拖入")
}

/// 应有译文的句数：**源文非空**的句子。
///
/// 空源句（静音段、纯空白行）没有任何可译内容，引擎也永远不会给它译文
/// （引擎挑待译句的口径就是 `!seg.translate_source().trim().is_empty()`）。
/// 若把它们算进分母，一次成功的整片翻译也会永远差几句，界面只能显示
/// 「部分完成」——分母必须与引擎的可译口径一致。
pub(crate) fn expected_translation_count(segments: &[Segment]) -> usize {
    segments
        .iter()
        .filter(|s| !s.translate_source().trim().is_empty())
        .count()
}

/// 当前目标语言下**实际译出**的句数：非空译文（`has_translation`）且语言匹配。
///
/// 两个条件缺一不可：只要译文是空串/纯空白（引擎解析失败时可能写入，照样带
/// `translation_lang`），就不算「译出」——这正是界面「已全部翻译」不能骗人的关键。
pub(crate) fn translated_out_count(segments: &[Segment], target: &str) -> usize {
    segments
        .iter()
        .filter(|s| s.has_translation() && s.translation_matches(target))
        .count()
}

/// 翻译收尾的统计口径（纯函数，便于单测）：`(实际译出句数, 应有译文句数)`。
///
/// 分子只认**目标语言 + 非空译文**（空串/纯空白不算），分母只算源文非空的句子。
pub(crate) fn translation_coverage(segments: &[Segment], target: &str) -> (usize, usize) {
    let done = translated_out_count(segments, target);
    (done, expected_translation_count(segments))
}

/// 把导入的字幕灌进工作台（同步、不 spawn），返回给用户看的提示文案；`segs`
/// 为空时置红色状态并返回 `None`。
///
/// 抽成不依赖 `MainWindow` 的纯状态函数有两个理由：一是「替换整份文档」的收尾
/// 步骤（重新编号、清撤销栈、清预览缓存、选首句、文案拼装）必须逐字一致；二是
/// 收尾里最关键的一步——替换前先把上一个工程的未保存编辑落库——能在单测里直接
/// 用一份内存 `AppState` 验证。
///
/// 为什么替换前必须 flush：导入的字幕会整份顶掉 `segments`，而 `segments` 里
/// 可能还压着用户没保存的编辑（`segments_dirty == true`）。跳过这一步，那些改动
/// 不但没进库，还会连 `active_task_id` 一起被置空，从此没有任何路径能把它们写
/// 回去——用户视角就是「导入一份字幕，上一份的修改全丢了」。
fn install_imported_subtitles(
    state: &mut AppState,
    path: &Path,
    mut segs: Vec<Segment>,
) -> Option<String> {
    if segs.is_empty() {
        state.status = ProcessStatus::Failed("字幕文件里没有可用的字幕行".to_string());
        return None;
    }
    // 句序号重新按输出顺序编号：外部文件的序号常常是错的或重复的，
    // 而下游（选中、质检、落库）全部按 `index` 索引。
    for (i, seg) in segs.iter_mut().enumerate() {
        seg.index = i + 1;
    }
    let count = segs.len();
    // 扩展名只取一次并统一成小写：`is_txt` / `is_lrc` 两处判断共用它，
    // 避免两份 `extension()` 拷贝各改各的而漂移。
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "导入的字幕".to_string());

    // 换文档前先把上一个工程的未保存编辑落库：flush 依赖**旧的** active_task_id，
    // 必须排在下面对 `segments` / `active_task_id` 的赋值之前，否则改动既没写进
    // 旧工程的库行、也失去了写回坐标，用户视角就是「导入一次丢一次修改」。
    state.flush_segments_if_dirty();

    state.segments = segs;
    // 换了文档就把目标语言对齐到这份字幕的语言：上次残留的「日本語」目标配
    // 一份日语字幕，点翻译等于日语译日语（引擎全判复制 → 整片失败）。
    state.adopt_source_language_for_translation();
    // 代次 +1：正在跑的翻译任务收尾时会比对它，避免把结果并进这份新文档。
    state.bump_doc_epoch();
    // 错别字记忆：把用户以前订正过的「错→对」应用进来（导入的字幕同样适用）。
    state.apply_typo_memory();
    // 这一步不是用户编辑：导入的文档本来就没有库主键，若留着脏标记，关窗守卫会
    // 弹出「改动会丢失」——而用户一个字都没改过。
    state.segments_dirty = false;
    // 导入的字幕不属于历史库里的任何一条记录：置空避免把编辑
    // 写回到别的工程的库行上（那是静默数据损坏）。
    state.active_task_id = None;
    state.selected_file = Some(path.to_path_buf());
    state.reset_edit_history();
    state.bump_segments_revision();
    state.clear_streaming();
    state.status = ProcessStatus::Idle;
    if let Some(first) = state.segments.first() {
        state.select_segment(first.index);
    }
    // 导入的是别人的字幕：波形/首帧都对不上，先清掉旧工程的缓存，
    // 免得监视器还显示上一支片子的画面。
    state.preview_source = None;
    state.waveform = None;
    state.active_tab = WorkspaceTab::Editor;

    Some(import_notice(&stem, count, ext == "txt", ext == "lrc"))
}

/// 转写线程退出后的兜底收尾：事件通道关闭时调用，只在状态仍停在 `Processing`
/// 时才动手，返回 `state.batch_running`（调用方据此决定要不要续跑批量队列）。
///
/// 正常跑完时管线一定先发 `Finished` 或 `Error`，事件分支已把状态收回 `Idle`，
/// 这里读到非 `Processing` 直接返回。真正需要兜底的是**线程异常退出**（典型是
/// panic）：没有终态事件、发送端析构、接收端读到 `None`，界面就会永远停在
/// 「转写中」，而且 `start_processing_for` 的 `Processing` 守卫会让用户再也开不了
/// 下一单，只能重启程序。
fn settle_aborted_transcription(state: &mut AppState) -> bool {
    if !state.is_processing() {
        return false;
    }
    if state.cancel_requested {
        // 用户主动终止：静默收尾，不弹完成框、不把「失败」写进历史库
        // （与 Finished/Error 两个分支里的取消处理逐字一致）。
        state.cancel_requested = false;
        state.status = ProcessStatus::Idle;
        state.transcribe_file = None;
        state.transcribe_duration = 0.0;
        state.clear_streaming();
        state.finish_active_queue_item(Err("已取消".to_string()));
        state.batch_running = false;
    } else if state.batch_running {
        // 批量中途线程异常退出：把当前这一条记为失败并解锁 UI，返回 `true`
        // 让调用方立刻续跑下一项——一个坏条目不该让整批永远卡住。
        state.transcribe_file = None;
        state.transcribe_duration = 0.0;
        state.clear_streaming();
        state.finish_active_queue_item(Err("转写意外中断：后台任务提前退出".to_string()));
    } else {
        // 单文件：没有队列可续，直接以失败态告知用户，让「开始转写」重新可点。
        state.clear_streaming();
        state.transcribe_file = None;
        state.transcribe_duration = 0.0;
        state.status = ProcessStatus::Failed("转写意外中断：运行时线程已退出".to_string());
    }
    state.batch_running
}

/// 清空全部译文：先快照、再改动、递增修订号、落库并记一条审计；没有译文时返回 `false`。
///
/// 快照必须在改动之前：`Ctrl+Z` 要把**清空之前**的整份译文换回来。审计记录用
/// `index = 0` 表示这不是针对单句的操作，`before` 写清空前的句数，日志面板
/// 据此能看出「这一条是整批清空」。
///
/// `segments_revision` 由本函数自己递增（见函数内注释）：清译文会改掉一批按修订号
/// 失效的派生量，失效责任收在改动点上，调用点不再手工补偿。
fn clear_translations_with_undo(state: &mut AppState) -> bool {
    let cleared = state
        .segments
        .iter()
        .filter(|s| s.translation.is_some())
        .count();
    if cleared == 0 {
        return false;
    }
    state.snapshot_for_undo();
    for seg in &mut state.segments {
        seg.translation = None;
        seg.translation_lang = None;
    }
    // 修订号必须在这里递增：清译文改掉了「是否还有双语行 / 译出句数 / 质检的未翻译」
    // 这些一律按 `segments_revision` 失效的派生量。修前本函数不递增，靠调用点
    // （`editor.rs` 里「清空译文」按钮那段）手工补一次——调用点一多，漏一个就会让
    // 清单继续按双语行高渲染、翻译卡仍说「已全部翻译」。补偿现在已删：责任收归
    // 本函数；双递增虽无副作用，却会让「谁负责失效」重新变模糊。
    state.bump_segments_revision();
    state.segments_dirty = true;
    state.flush_segments_if_dirty();
    state.edit_log.push(EditRecord {
        index: 0,
        kind: EditKind::Translation,
        before: format!("{cleared} 句译文"),
        after: String::new(),
        at: chrono::Local::now(),
    });
    true
}

/// 「译文被模型原样复制回原文」的判定：直接复用翻译管线的告警判据
/// （[`crate::engines::llm::is_untranslated_copy`]）。
///
/// 刻意不在这里另写一条「trim 后相等」：管线那条判据带着必要的**放行**——原文短于
/// 4 个字符、或原文不含任何 CJK（`OK`、`P(A+B)`、纯数字、纯拉丁术语本来就该原样
/// 保留）。两处口径一旦分家，用户会看到「这个按钮说没有复制句，而翻译卡告警说
/// 检出 N 句复制」的自相矛盾画面。
pub(crate) fn is_copied_translation(seg: &Segment) -> bool {
    let Some(translation) = seg.translation.as_deref() else {
        return false;
    };
    crate::engines::llm::is_untranslated_copy(seg.display_text(), translation)
}

/// 译文被判为「复制原文」的句在 `segments` 里的**位置**下标。
///
/// 位置（数组下标）与句序号（`Segment::index`）是两套坐标：这里是给「按位置改
/// `segments`」用的，审计/报告里的 `index` 仍是句序号，两者别混。
pub(crate) fn copied_translation_positions(segments: &[Segment]) -> Vec<usize> {
    segments
        .iter()
        .enumerate()
        .filter(|(_, seg)| is_copied_translation(seg))
        .map(|(pos, _)| pos)
        .collect()
}

/// 是否存在「译文与原文一字不差」的句子（界面据此决定要不要给出清理入口）。
pub(crate) fn has_copied_translations(segments: &[Segment]) -> bool {
    segments.iter().any(is_copied_translation)
}
/// 用面板里的「查找」文本给规则集起名（纯函数，便于单测）。
///
/// 规则集名字要能一眼看出它是干什么用的，而面板里唯一现成的语义就是查找串；
/// 截断到 20 个字符，免得超长查找串把下拉列表撑爆。
fn rule_set_name_for(find: &str) -> String {
    let trimmed = find.trim();
    let taken: String = trimmed.chars().take(20).collect();
    if trimmed.chars().count() > 20 {
        format!("规则：{taken}…")
    } else {
        format!("规则：{taken}")
    }
}

/// 规则集应用结果的提示文案（纯函数，便于单测）。
///
/// 与面板里「全部替换」的文案同口径（处数 + 涉及句数，译文命中单独点名），
/// 免得同一件事在两条入口给出两种说法。
fn replace_rule_report_message(name: &str, report: &ReplaceRuleReport) -> String {
    let mut msg = format!(
        "规则集「{name}」已替换 {} 处，涉及 {} 句",
        report.total_hits(),
        report.changed.len()
    );
    if report.translation_hits > 0 {
        msg.push_str(&format!("（其中译文 {} 处）", report.translation_hits));
    }
    msg
}

/// 自动补下载的尝试次数（进程级）。防「下载失败 → 完成回调立刻再试」这种死循环：
/// 网络不通时每次尝试都要等一轮超时，不设上限会让界面永远在转圈。
static REQUIRED_AUTO_TRIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// 最多自动尝试几次（含首次）。到顶后只提示一次、转人工。
const REQUIRED_AUTO_MAX_TRIES: u32 = 3;

impl MainWindow {
    /// 裁剪视频库的界面缓存，只保留仍存在于列表里的条目。
    ///
    /// `library_thumbs`（首帧缩略图路径）与 `library_sizes`（文件大小文本）
    /// 都按 `task_id` 累积，而 `recent_tasks` 只保留最近 50 条。用户长期
    /// 使用（不断导入/删除）后，这两个 map 会一直涨且全是查不到的僵尸条目。
    /// 在每次刷新列表后统一按当前列表裁剪，是最省心的一处收口。
    pub(crate) fn prune_library_caches(&mut self) {
        let live: std::collections::HashSet<i64> =
            self.state.recent_tasks.iter().map(|t| t.id).collect();
        self.library_thumbs.retain(|id, _| live.contains(id));
        self.library_sizes.retain(|id, _| live.contains(id));
        // 选中集同步裁掉已不存在的任务：删除/刷新后不该再留着查不到的 id。
        self.library_selected.retain(|id| live.contains(id));
    }

    /// 把视频库里勾选的多个任务一次性导出到同一目录。
    ///
    /// 与单条「导出字幕」同源：字幕按 id 现取（列表记录不含正文），导出内容模式
    /// 沿用导出栏的「原文 / 仅译文 / 双语」，导出格式沿用配置抽屉的
    /// 「字幕输出格式」（经 [`export_spec_for`]，未知值回落 srt）。逐条写盘，
    /// 失败的记下来一次性汇总提示，不让一条坏数据中断整批。
    pub(crate) fn export_selected_library_tasks(&mut self, cx: &mut Context<Self>) {
        if self.library_export_busy {
            return;
        }
        // 按列表顺序取出选中项（保证导出提示与视觉顺序一致）
        let picked: Vec<(i64, String)> = self
            .state
            .recent_tasks
            .iter()
            .filter(|t| self.library_selected.contains(&t.id))
            .map(|t| (t.id, t.file_name.clone()))
            .collect();
        if picked.is_empty() {
            return;
        }

        let export_mode = self.state.export_mode_from_config();
        // 格式在派发前就定下来：整批共用同一份 spec（同一扩展名、同一写出格式），
        // 与单条「导出字幕」走的是同一个 helper，不再各写一份字面量。
        // 文件名后缀取**扩展名**、写盘参数取**规范格式名**：`txt-ts`（带时间码纯文本）
        // 的扩展名就是 `txt`，拿扩展名当格式名会把「带时间码」静默写成一堆纯文本。
        let write_fmt = crate::subtitle::writer::canonical_write_format(&self.state.output_format);
        let (fmt_ext, _fmt_label) = export_spec_for(write_fmt);
        // 样式同样在派发前 clone 出来（与剪辑台 `perform_editor_export` 同法）：
        // 后台闭包是 `'static` 的，拿不到 `this`，必须先复制一份。此前批量导出
        // 调用的是无样式的 `write_to_file_with_mode`，用户在配置里把单行最大字数
        // 调小后，库里批量导出的字幕依旧不折行（中→英译文本会超屏），
        // 与剪辑台逐条导出的结果不一致。
        let style = self.state.config.subtitle_style.clone();
        // 文件名模板与日期同样要在派发前抽出来：`background_executor().spawn` 的
        // 闭包必须 `Send`，闭包里读 `self.state.config` 会把 `self` 拖进去。
        let name_template = self.state.config.ui.export_name_template.clone();
        let today = chrono::Local::now().format("%Y%m%d").to_string();
        let db = self.state.db.clone();
        self.library_export_busy = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let folder = rfd::AsyncFileDialog::new()
                .set_title("选择批量导出目录")
                .pick_folder()
                .await;
            let Some(folder_handle) = folder else {
                // 用户取消：复位忙状态，不弹提示
                let _ = this.update(cx, |this, cx| {
                    this.library_export_busy = false;
                    cx.notify();
                });
                return;
            };
            let dir = folder_handle.path().to_path_buf();
            // 后台闭包与收尾提示都要用 dir：各持一份，避免 move 冲突
            let dir_for_write = dir.clone();

            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut ok = 0usize;
                    let mut failed: Vec<String> = Vec::new();
                    for (id, name) in &picked {
                        let segs = match db.load_task_segments(*id) {
                            Ok(s) if !s.is_empty() => s,
                            Ok(_) => {
                                failed.push(format!("{name}（无字幕内容）"));
                                continue;
                            }
                            Err(e) => {
                                failed.push(format!("{name}（读取失败：{e}）"));
                                continue;
                            }
                        };
                        // 文件名沿用工程名，剥掉原扩展名再补上所选格式的扩展名，
                        // 避免「xx.mp4.srt」。此前扩展名与写出格式都硬编码为
                        // "srt"，用户在配置里选了 JSON，批量导出却仍是 SRT 文件。
                        let stem = std::path::Path::new(name)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| format!("task_{id}"));
                        // 文件名走统一模板：库批量导出与剪辑台单条导出必须给出
                        // 同一套命名规则，否则同一个配置在两个入口行为不同。
                        let fname = crate::subtitle::writer::export_file_name(
                            &name_template,
                            &stem,
                            fmt_ext,
                            &today,
                        );
                        let out = dir_for_write.join(&fname);
                        // 走带样式的入口：srt / vtt / ass 按 `max_chars_per_line` 折行。
                        // 其余格式（json / ttml / ttal / txt）在
                        // `write_to_file_with_style` 内部**原样回落**到
                        // `write_to_file_with_mode`，不会因为这次接线而报错或改变字节。
                        match SubtitleWriter::write_to_file_with_style(
                            &segs,
                            &out,
                            write_fmt,
                            export_mode,
                            &style,
                        ) {
                            Ok(()) => ok += 1,
                            Err(e) => failed.push(format!("{name}（写出失败：{e}）")),
                        }
                    }
                    (ok, failed)
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.library_export_busy = false;
                let (ok, failed) = result;
                if failed.is_empty() {
                    this.notice = Some(format!("已导出 {ok} 份字幕到 {}", dir.display()));
                } else {
                    let preview = failed
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("；");
                    let more = if failed.len() > 3 {
                        format!("等 {} 项", failed.len())
                    } else {
                        String::new()
                    };
                    this.notice = Some(format!(
                        "批量导出完成：成功 {ok} 份，失败 {} 项（{preview}{more}）",
                        failed.len()
                    ));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// CPU 机器强制走 H.264 代理；有 GPU 默认原片，用户仍可手动打开。
    pub(crate) fn ensure_preview_proxy(&mut self, cx: &mut Context<Self>) {
        self.probe_video_dimensions(cx);
        if !self.state.proxy_enabled {
            self.state.preview_source = self.state.selected_file.clone();
            self.state.proxy_busy = false;
            return;
        }
        let Some(source) = self.state.selected_file.clone() else {
            return;
        };
        if self.state.proxy_busy {
            return;
        }
        let cpu = !self.state.hardware.use_gpu_pipeline();
        let height = self.state.proxy_manager.preview_height(&source, cpu);
        if let Some(existing) = self.state.proxy_manager.existing_proxy(&source, height) {
            self.state.preview_source = Some(existing);
            self.state.proxy_busy = false;
            return;
        }
        self.state.proxy_busy = true;
        let proxy = self.state.proxy_manager.clone();
        info!(path = %source.display(), height, "后台生成 H.264 预览代理");
        // 预览代理是**整片** FFmpeg 转码，耗时随视频时长线性增长（分钟级），
        // 属于任务清单里的长任务 v2w-preview-proxy（见 utils::background_thread）。
        // 不能走 `background_executor`：Windows 上 GPUI 把每个后台任务都按 High
        // 派发，会和转写中的 whisper 抢 CPU。改走专用线程 + BELOW_NORMAL。
        let result_rx =
            crate::utils::background_thread::spawn_long_task("v2w-preview-proxy", move || {
                proxy.ensure_proxy(&source, height)
            });
        cx.spawn(async move |this, cx| {
            let result = match result_rx.await {
                Ok(result) => result,
                // 线程起不来 / panic：按「代理生成失败」处理，预览回退原片。
                Err(_) => Err(anyhow::anyhow!("预览代理线程异常退出")),
            };
            let _ = this.update(cx, |this, cx| {
                this.state.proxy_busy = false;
                match result {
                    Ok(path) => {
                        this.state.preview_source = Some(path);
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "代理生成失败，预览回退原片");
                        this.state.preview_source = this.state.selected_file.clone();
                    }
                }
                this.trigger_extract_frame(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开系统原生文件对话框选择音视频（支持单选与多选，异步非阻塞）
    pub(crate) fn choose_file(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handles = rfd::AsyncFileDialog::new()
                .add_filter(
                    "音视频文件",
                    &[
                        "mp4", "mkv", "mov", "avi", "flv", "webm", "mp3", "wav", "flac", "m4a",
                    ],
                )
                .pick_files()
                .await;
            let Some(handles) = handles else { return };
            if handles.is_empty() {
                return;
            }
            let files: Vec<PathBuf> = handles.iter().map(|h| h.path().to_path_buf()).collect();
            let _ = this.update(cx, |this, cx| {
                this.handle_selected_media_files(files, cx);
            });
        })
        .detach();
    }

    /// 处理用户通过文件对话框选择的音视频文件（支持单选与多选，平滑衔接单文件与批量队列）
    pub(crate) fn handle_selected_media_files(
        &mut self,
        files: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if files.is_empty() {
            return;
        }
        // 若多选、或当前批量队列已有项目：一律作为批量队列处理
        if files.len() > 1 || !self.state.batch_queue.is_empty() {
            // 如果单文件就绪卡片上先前已选中过一个文件，顺带并入批量队列，绝不丢弃
            if let Some(prev) = self.state.transcribe_file.take() {
                self.state.enqueue_files(vec![prev]);
            }
            let count = files.len();
            self.ingest_paths(files, cx);
            if let Some(first) = self.state.batch_queue.first() {
                self.state.transcribe_file = Some(first.path.clone());
                self.state.selected_file = Some(first.path.clone());
                self.state.transcribe_duration = first.duration;
            }
            self.queue_sidebar_open = true;
            self.state.active_tab = WorkspaceTab::Generate;
            self.notice = Some(format!("已添加 {count} 个视频到批量转写队列"));
            cx.notify();
        } else {
            // 恰好 1 个文件且当前无队列：作为单文件就绪
            self.adopt_media_file(files[0].clone(), cx);
        }
    }

    /// 将单文件就绪卡片转为批量队列并打开选择器添加更多视频
    #[allow(dead_code)]
    pub(crate) fn convert_ready_to_batch(&mut self, cx: &mut Context<Self>) {
        if let Some(file) = self.state.transcribe_file.take() {
            self.state.enqueue_files(vec![file.clone()]);
            self.state.transcribe_file = Some(file);
            self.probe_queue_durations(cx);
        }
        self.queue_sidebar_open = true;
        self.choose_batch_files(cx);
    }

    /// 删除某个可下载组件的本地文件（「模型与组件」卡的「删除」按钮走这里）。
    ///
    /// 删除逻辑**全部**委托给 `utils::model_download::delete_item_file`：
    /// 那边是唯一的路径解析 + 安全校验处（拒绝压缩包类组件、拒绝目录、拒绝用户
    /// 自编译构建），这里只负责把结果翻译成界面反馈，绝不在 UI 层再拼一遍路径——
    /// 两份实现一旦漂移，就会出现「界面允许删、底层拒绝」或更糟的「删错东西」。
    ///
    /// 反馈落在 `download_status_msg`（模型卡里就地显示，成功/失败/中性三色），
    /// 而不是 `state.status`：后者是红色横幅、且只在转写页可见，用户在性能页
    /// 点删除根本看不到。
    pub(crate) fn delete_model_file(&mut self, item_id: &str, cx: &mut Context<Self>) {
        let label = crate::utils::model_download::item_by_id(item_id)
            .map(|i| i.label)
            .unwrap_or(item_id);
        match crate::utils::model_download::delete_item_file(item_id, &self.state.config) {
            Ok(Some(path)) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                self.state.download_status_msg = format!("已删除 {name}");
                // 删完立刻重扫就位状态：徽标要从「已就位」翻回「下载」，
                // 否则用户会以为没删掉（卡片还显示已就位）。重扫对声明了 sha256
                // 的条目会全文件哈希（同样是长任务），走专用线程。
                self.refresh_model_presence_offloaded(cx);
            }
            Ok(None) => {
                self.state.download_status_msg = format!("{label}：磁盘上没有对应文件，无需删除");
                // 同「删除成功」分支：重扫走专用线程（见上）。
                self.refresh_model_presence_offloaded(cx);
            }
            Err(e) => {
                self.state.download_status_msg = format!("删除失败：{e}");
            }
        }
        cx.notify();
    }

    /// 一键备份数据目录（库 + 配置）到 `backups/` 下的时间戳归档。
    ///
    /// 为什么要有它：整个工程库就是一个 `voice2word.db`，误删或想换机器时用户
    /// 手上没有任何快照；而**运行中**手工复制这个文件是不安全的——SQLite 的 WAL
    /// 是独立文件，只拷 `.db` 会丢掉最近几次编辑。备份走 `utils::backup`
    /// （连 `-wal`/`-shm` 一起打包），把「安全的复制方式」固化成一次点击。
    ///
    /// 全程在后台线程做（打包几百 MB 的库会阻塞 UI）；完成后把归档路径写进
    /// `download_status_msg` 并**在资源管理器里选中它**，让用户立刻看到产物在哪。
    pub(crate) fn backup_data(&mut self, cx: &mut Context<Self>) {
        if self.state.is_downloading {
            return;
        }
        let root = crate::utils::AppConfig::app_root_dir();
        let name = crate::utils::backup::backup_file_name(chrono::Local::now());
        let dest = root.join("backups").join(&name);
        // 库的真实位置：项目根**或**回退目录（装在只读目录时库会退到
        // %LOCALAPPDATA%\Voice2Word）。备份必须按**真实位置**取，否则会出现
        // 「备份成功但里面没有数据库」——用户拿到的归档里没有历史记录，
        // 却什么提示都没有。
        let db_path = crate::utils::AppConfig::resolve_path("voice2word.db");
        let db_path = if db_path.is_file() {
            db_path
        } else {
            let fallback = crate::utils::AppConfig::writable_data_dir().join("voice2word.db");
            if fallback.is_file() {
                fallback
            } else {
                db_path
            }
        };
        self.state.download_status_msg = "正在备份数据…".to_string();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::utils::backup::create_backup_with_db(&root, &db_path, &dest)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(outcome) => {
                        info!("数据备份完成: {}", outcome.archive.display());
                        this.state.download_status_msg = format!(
                            "已备份 {} 个文件（{}）到 backups/",
                            outcome.included.len(),
                            crate::utils::model_download::human_size(outcome.bytes)
                        );
                        // 打开资源管理器并选中归档：用户下一步多半就是把它拷走。
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{}", outcome.archive.display()))
                            .spawn();
                    }
                    Err(e) => {
                        this.state.download_status_msg = format!("备份失败：{e}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 选择一份备份归档并（在二次确认后）恢复。
    ///
    /// 恢复会**写回** `voice2word.db` 等文件，因此先弹确认框；确认后才真正执行
    /// （走 [`MainWindow::restore_data_backup`]）。底层 `utils::backup::restore_backup`
    /// 本身拒绝覆盖已存在的文件——这是有意的双保险：即便用户误点确认，也不会
    /// 悄悄把当前工程库换掉；界面会把「哪些文件因为已存在而被保留」如实报出来。
    pub(crate) fn choose_backup_to_restore(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择要恢复的备份归档")
                .add_filter("Voice2Word 备份", &["zip"])
                .pick_file()
                .await;
            let Some(handle) = handle else { return };
            let path = handle.path().to_path_buf();
            let _ = this.update(cx, |this, cx| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                this.confirm_dialog = Some(crate::ui::types::ConfirmDialogInfo {
                    title: "从备份恢复？".to_string(),
                    message: format!(
                        "将用「{name}」里的内容恢复数据目录。为保护你当前的工程，**已存在的同名文件不会被覆盖**——\
                         若确实要用备份替换，请先手动把现有 voice2word.db / config.toml 改名。"
                    ),
                    confirm_label: "恢复".to_string(),
                    danger: true,
                    action: crate::ui::types::ConfirmAction::RestoreBackup(path),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 执行备份恢复（确认后调用）。结果同样落在 `download_status_msg`。
    pub(crate) fn restore_data_backup(
        &mut self,
        archive: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let root = crate::utils::AppConfig::app_root_dir();
        // 恢复目标必须与「库的真实位置」一致：项目根可写时是项目根，
        // 项目根只读时库在 %LOCALAPPDATA%\Voice2Word，恢复也应该写回那里——
        // 否则用户在一个地方恢复、程序却从另一个地方读，表现为「恢复没生效」。
        let target_root = {
            let db = root.join("voice2word.db");
            if db.is_file() || crate::utils::AppConfig::dir_is_writable(&root) {
                root.clone()
            } else {
                crate::utils::AppConfig::writable_data_dir()
            }
        };
        match crate::utils::backup::restore_backup(archive, &target_root) {
            Ok(outcome) => {
                let mut msg = if outcome.restored.is_empty() {
                    "没有可恢复的文件".to_string()
                } else {
                    format!("已恢复 {} 个文件", outcome.restored.len())
                };
                if !outcome.skipped_existing.is_empty() {
                    // 这条提示是恢复功能的**核心安全语义**：用户以为「恢复=替换」，
                    // 实际是「只补缺失」。不说清他会以为恢复失败或数据没变。
                    msg.push_str(&format!(
                        "；{} 已存在，已保留现有版本（如需替换请先手动改名）",
                        outcome.skipped_existing.join("、")
                    ));
                }
                // 数据库只在启动时打开一次（`main.rs`），运行中恢复的新库不会被当前
                // 进程读取。以前只说「已恢复 N 个文件」，用户以为生效了，接着把字幕
                // 编辑写进了那份已被替换掉的旧库。必须明确要求重启。
                if outcome.restored.iter().any(|f| f.contains("voice2word.db")) {
                    msg.push_str("；请**退出并重新启动** Voice2Word，恢复的数据库才会被加载");
                }
                self.state.download_status_msg = msg;
                // 恢复的是库与配置：下次启动才读，这里只刷新界面上的就位/库视图。
                // 重扫同样可能触发 sha256 全文件读（长任务），走专用线程。
                self.refresh_model_presence_offloaded(cx);
            }
            Err(e) => {
                self.state.download_status_msg = format!("恢复失败：{e}");
            }
        }
        cx.notify();
    }

    /// 打开模型/工具的落地根目录（`models/` 与 `tools/` 所在的目录）。    /// 打开模型/工具的落地根目录（`models/` 与 `tools/` 所在的目录）。
    ///
    /// 「打开目录」是为了兜住 `delete_item_file` 明确不肯删的那一类：压缩包组件
    /// （whisper-cli / llama.cpp）解压后铺在 `tools/` 下、与共享 DLL 混在一起，
    /// 程序没法安全地替用户删。与其让用户对着提示去手敲路径，不如给一个按钮直接
    /// 把目录甩到资源管理器里。
    /// 导出**质检复核表**（CSV / Markdown），供人工复核或交给他人。
    ///
    /// # 为什么需要它
    ///
    /// 质检结论此前只活在界面里：复核的人只能在剪辑台上一个个点，也没法把「哪些句
    /// 有问题」交给别人。复核表把结论变成 Excel 能开、工单能贴的东西。
    ///
    /// 走系统「保存文件」对话框而不是固定路径：复核表是给**人**用的交付物，
    /// 落点由用户决定（他要发给谁就放哪）。
    pub(crate) fn export_review_sheet(
        &mut self,
        format: crate::subtitle::qc::QcFormat,
        cx: &mut Context<Self>,
    ) {
        use crate::subtitle::segment::quality_report;
        if self.state.segments.is_empty() {
            return;
        }
        // 与转写页质检卡同一份判据（阈值取自配置、术语表取自缓存），保证导出的
        // 数字与界面上看到的一致——两处各算一遍必然出现「界面 3 句、导出 5 句」。
        let check_untranslated = self.state.translated_count() > 0;
        let threshold = self.state.config.pipeline.whisper_low_confidence;
        let mut report = quality_report(&self.state.segments, &[], threshold, check_untranslated);
        report.glossary_violations = self.cached_glossary_violations();
        let flagged = report.all_issues().len();
        let total = self.state.segments.len();
        let sheet = crate::subtitle::qc::render_review_sheet(&self.state.segments, &report, format);
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "subtitle".to_string());
        let file_name = format!(
            "{}.{}",
            crate::subtitle::qc::review_file_stem(&stem),
            format.extension()
        );

        // 空表也给导出：用户点了按钮就该拿到文件（「没有问题」本身是结论），
        // 但要在提示里说清有几行，免得他以为导出漏了。
        self.notice = Some(format!("已导出质检表（{flagged} 行待复核，共 {total} 句）"));
        self.export_text_content(cx, file_name, sheet, format.label());
    }

    /// 把「上次崩溃」的完整报告（含堆栈、版本、环境）复制到剪贴板。
    ///
    /// 为什么要有这个入口：崩溃横幅以前只给一行摘要 + 一个「导出诊断」，而诊断报告
    /// 里**没有堆栈**——用户报障时最关键的那段恰好拿不到。这里直接把
    /// `crash_report::build_report` 的产物放进剪贴板，贴进聊天窗口即可。
    pub(crate) fn copy_crash_report(&mut self, cx: &mut Context<Self>) {
        let Some(record) = self.crash_record.as_ref() else {
            self.notice = Some("没有可复制的崩溃记录".to_string());
            cx.notify();
            return;
        };
        let report = crate::utils::crash_report::build_report(record, env!("CARGO_PKG_VERSION"));
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(report));
        self.notice = Some("已复制崩溃报告（含堆栈），可直接粘贴反馈".to_string());
        cx.notify();
    }
    /// 检查视频库里的重复记录，并把结果作为中性提示告诉用户。
    ///
    /// # 为什么只提示、不自动清理
    ///
    /// 自动删掉「重复」是危险动作：指纹是**采样**哈希（见 `utils::fingerprint` 的
    /// 模块文档），理论上不同的文件可能撞上；更要紧的是用户可能刻意保留同内容的两条
    /// 记录（不同字幕版本、不同目标语言）。所以这里只回答「哪些是同一份媒体」，
    /// 删不删、删哪条由用户决定——`suggested_keeper` 给出建议（保留最早那条，
    /// 它可能已经人工校对过），但按钮不会替他按下。
    pub(crate) fn report_library_duplicates(&mut self, cx: &mut Context<Self>) {
        use crate::utils::duplicates::{
            describe_scan, fingerprinted_count, group_by_fingerprint, suggested_removals,
        };
        let groups = group_by_fingerprint(&self.state.recent_tasks);
        // 一条指纹都没有时，「没有发现重复记录」是假阴性（没测 ≠ 通过），
        // 由 `describe_scan` 统一区分，见那里的说明。
        let mut msg = describe_scan(&groups, fingerprinted_count(&self.state.recent_tasks));
        if let Some(first) = groups.first() {
            // 附一个具体例子：只说「有 3 组重复」用户不知道从哪下手。
            let sample: Vec<String> = first.names.clone();
            msg.push_str(&format!("；例如「{}」", sample.join("」与「")));
            let removals = suggested_removals(first).len();
            if removals > 0 {
                msg.push_str(&format!("（该组可清理 {removals} 条）"));
            }
        }
        // 落在 `notice`（中性）而不是 `status`（红色错误）：重复不是故障，
        // 是「你可能想清理一下」的信息。
        self.notice = Some(msg);
        cx.notify();
    }

    /// 检查是否有新版本（用户显式点击才发请求，不轮询）。
    ///
    /// # 为什么不做成自动检查
    ///
    /// 启动即发一个出站请求，对「完全本地运行」是这个项目的核心卖点之一的工具来说
    /// 是背道而驰的；而且 GitHub 未认证 API 有速率限制，自动轮询很容易把自己查 403。
    /// 所以这里只在用户点「检查更新」时请求一次，结果缓存进会话状态。
    ///
    /// 只**报告**、不下载也不替换 exe：静默替换正在运行的可执行文件是能把安装搞死
    /// 的操作，那属于单独的、更高风险的一步（见 `utils::update_check` 的模块文档）。
    /// 检查是否有新版本（用户显式点击才发请求，不轮询）。
    pub(crate) fn check_for_update(&mut self, cx: &mut Context<Self>) {
        if self.state.update_check_busy {
            return;
        }
        self.state.update_check_busy = true;
        self.state.download_status_msg = "正在检查更新…".to_string();
        self.state.update_download_status = Some("正在连接 GitHub 检查最新版本…".to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::utils::update_check::fetch_latest_release(15) })
                .await;
            let _ = this.update(cx, |this, cx| {
                use crate::utils::update_check::{current_version, describe, is_newer};
                this.state.update_check_busy = false;
                match result {
                    Ok(latest) => {
                        let current = current_version();
                        let newer = is_newer(&latest.version, current);
                        let text = describe(current, &latest.version, newer);
                        this.state.download_status_msg = text.clone();
                        this.state.update_release_info = Some(latest.clone());
                        this.state.update_check_result =
                            Some((newer, text.clone(), latest.page_url));
                        this.state.update_download_target =
                            Some((latest.download_url, latest.asset_name, latest.asset_size));
                        this.state.update_download_status = Some(text);

                        // 发现新版本时，自动弹出更新详情弹窗，便于用户查看改动并一键下载
                        if newer {
                            this.update_modal_open = true;
                        }
                    }
                    Err(e) => {
                        let err_msg = format!("检查更新失败：{e}");
                        this.state.download_status_msg = err_msg.clone();
                        this.state.update_download_status = Some(err_msg);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 应用内直接下载最新版本（支持国内加速镜像，打通实时进度流）
    pub(crate) fn download_app_update(&mut self, cx: &mut Context<Self>) {
        if self.state.update_download_busy {
            return;
        }
        let Some((url, name, _)) = self.state.update_download_target.clone() else {
            return;
        };
        if url.trim().is_empty() {
            return;
        }

        self.state.update_download_busy = true;
        self.state.update_download_progress = 0.01;
        self.state.update_download_status = Some("正在连接更新镜像…".to_string());
        self.update_download_cancel
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let cancel = self.update_download_cancel.clone();
        cx.notify();

        // 建立跨线程实时下载进度通信通道
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(f32, u64, u64)>();

        // 监听进度通道并热刷新 UI
        cx.spawn(async move |this, cx| {
            while let Some((frac, written, total)) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.state.update_download_progress = frac;
                    let mb_done = written as f64 / 1024.0 / 1024.0;
                    if total > 0 {
                        let mb_total = total as f64 / 1024.0 / 1024.0;
                        this.state.update_download_status = Some(format!(
                            "正在下载更新：{:.1} MB / {:.1} MB ({:.0}%)",
                            mb_done,
                            mb_total,
                            frac * 100.0
                        ));
                    } else {
                        this.state.update_download_status =
                            Some(format!("正在下载更新：{:.1} MB", mb_done));
                    }
                    cx.notify();
                });
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    crate::utils::update_check::download_update_file(
                        &url,
                        &name,
                        &cancel,
                        move |frac, written, total| {
                            let _ = tx.send((frac, written, total));
                        },
                    )
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.state.update_download_busy = false;
                match res {
                    Ok(path) => {
                        this.state.update_download_progress = 1.0;
                        let msg = format!("新版本已下载就绪：{}", path.display());
                        this.state.update_download_status = Some(msg);
                        this.state.update_downloaded_path = Some(path);
                    }
                    Err(e) => {
                        this.state.update_download_status = Some(format!("下载失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 取消更新包下载
    pub(crate) fn cancel_update_download(&mut self, cx: &mut Context<Self>) {
        if self.state.update_download_busy {
            self.update_download_cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.state.update_download_busy = false;
            self.state.update_download_status = Some("已取消下载".to_string());
            cx.notify();
        }
    }

    /// 安装已下载的新版本并退出当前应用
    pub(crate) fn install_downloaded_update(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.state.update_downloaded_path.clone() else {
            return;
        };
        if !path.exists() {
            self.state.update_download_status = Some("更新文件不存在，请重新下载".to_string());
            cx.notify();
            return;
        }

        // 保存未保存的字幕数据。**必须看返回值**：flush 失败（库被外部锁住、
        // 磁盘满）或存在未保存的导入草稿时不能退出——否则点「立即退出并安装升级」
        // 就等于把最后一段字幕直接丢掉，与关窗守卫的取舍自相矛盾。
        if !self.flush_segments_before_close(cx) {
            self.state.update_download_status = Some(
                "还有未保存的字幕改动，已取消退出更新。请先按提示条处理好保存问题（或先导出字幕）再重试。"
                    .to_string(),
            );
            cx.notify();
            return;
        }

        #[cfg(target_os = "windows")]
        {
            let res = std::process::Command::new(&path).spawn();
            if let Err(e) = res {
                self.state.update_download_status = Some(format!("启动更新程序失败：{e}"));
                cx.notify();
                return;
            }
            cx.quit();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = open::that(&path);
        }
    }

    /// 打开已下载的新版文件所在文件夹
    pub(crate) fn open_downloaded_update(&mut self, _cx: &mut Context<Self>) {
        if let Some(path) = &self.state.update_downloaded_path {
            Self::reveal_path_in_explorer(path);
        }
    }

    /// 在浏览器里打开最新版本的下载页（`update_check_result` 里有 URL）。
    pub(crate) fn open_release_page(&mut self, cx: &mut Context<Self>) {
        let Some((_, _, url)) = self.state.update_check_result.clone() else {
            return;
        };
        if url.trim().is_empty() {
            return;
        }
        let _ = open::that(&url);
        cx.notify();
    }

    /// 导出**编辑日志**（本次会话改了哪些句、改前改后是什么）。
    ///
    /// # 为什么需要它
    ///
    /// 撤销栈解决的是「退回去」，解决不了「改了什么」：它只有 50 层、换文档即清空、
    /// 退出即丢。用户交片给客户、或两周后回头看自己的工程，都需要能查到「这句当时
    /// 是什么、谁改的、什么时候」。
    ///
    /// 展示前先 `coalesced(60)`：文本编辑逐键记一条，不折叠的话单个句子就能刷满
    /// 整张表。**导出的是折叠后的视图**，折叠只是展示取舍，原始记录仍在内存里。
    pub(crate) fn export_edit_log(
        &mut self,
        format: crate::subtitle::qc::QcFormat,
        cx: &mut Context<Self>,
    ) {
        use crate::subtitle::audit::{render_audit, DEFAULT_COALESCE_SECS};
        let log = self.state.edit_log.coalesced(DEFAULT_COALESCE_SECS);
        if log.is_empty() {
            // 空日志也导出：用户点了就该拿到文件（「本次没有编辑」本身是结论）。
            self.notice = Some("本次会话没有编辑记录，已导出空表".to_string());
        } else {
            self.notice = Some(format!("已导出编辑日志（{} 条记录）", log.len()));
        }
        let sheet = render_audit(log.records(), format);
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "subtitle".to_string());
        let file_name = format!("{stem}-编辑日志.{}", format.extension());
        self.export_text_content(cx, file_name, sheet, format.label());
    }

    /// 导出**诊断报告**：把「程序实际解析到的路径、每个组件的状态、运行库能否解析」
    /// 打成一份可复制的纯文本。
    ///
    /// # 为什么需要它
    ///
    /// 「说 ffmpeg 缺失，可我明明有 ffmpeg」这类问题此前只能靠零散的日志行来回猜：
    /// 用户看不到程序把项目根锚到了哪、每个可执行文件实际是哪一个、它的依赖 DLL 是否
    /// 能解析。报告把这些一次问清楚，用户直接复制粘贴就能求助，把几天来回缩短到一条消息。
    ///
    /// 与质检表共用 `export_text_content`（同一套「另存为 + 后台写盘 + 选中文件」），
    /// 不另造导出链路。
    pub(crate) fn export_diagnostics(&mut self, cx: &mut Context<Self>) {
        let root = crate::utils::AppConfig::app_root_dir();
        let report = crate::utils::diagnostics::build_report(&self.state.config, &root);
        // 文件名带日期：用户可能对比「改配置前后」两份报告，带日期才分得清。
        let name = format!(
            "voice2word-diagnostics-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M")
        );
        self.notice = Some("已导出诊断报告，可直接复制粘贴用于排查".to_string());
        self.export_text_content(cx, name, report, "诊断报告");
    }

    /// 把一段纯文本内容交给「另存为」对话框写出（质检表这类非字幕产物的共用出口）。    /// 把一段纯文本内容交给「另存为」对话框写出（质检表这类非字幕产物的共用出口）。
    ///
    /// 单独抽出来而不是复用 `export_with_save_dialog`：后者绑定的是「字幕片段 +
    /// SubtitleWriter」这条链路，而质检表是**已经渲染好的字符串**，再套一层写盘
    /// 适配器只会把两件不相干的事绕在一起。
    fn export_text_content(
        &mut self,
        cx: &mut Context<Self>,
        default_name: String,
        content: String,
        filter_label: &str,
    ) {
        let filter = filter_label.to_string();
        cx.spawn(async move |this, cx| {
            let ext = default_name.rsplit('.').next().unwrap_or("txt").to_string();
            let handle = rfd::AsyncFileDialog::new()
                .set_title("导出质检表")
                .set_file_name(&default_name)
                .add_filter(&filter, &[ext.as_str()])
                .save_file()
                .await;
            let Some(handle) = handle else { return };
            let path = handle.path().to_path_buf();
            // 写盘本身是毫秒级小文件，但保持与其它导出一致的「后台执行」习惯，
            // 免得将来内容变大时又要改一遍。路径 clone 一份进后台闭包：`path`
            // 在收尾的成功分支里还要用（打开资源管理器选中它）。
            let path_for_write = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { std::fs::write(&path_for_write, content.as_bytes()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        // 打开所在目录并选中文件：用户下一步多半就是把它发出去。
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{}", path.display()))
                            .spawn();
                    }
                    Err(e) => {
                        this.state.status = ProcessStatus::Failed(format!("质检表写出失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 在系统资源管理器中定位文件并选中；若为目录或文件不存在则打开所在目录。
    ///
    /// # Windows 下的特殊处理
    /// Windows Explorer 的 `/select,<path>` 命令行对斜杠非常敏感：如果路径中含有正斜杠 `/`
    /// 或被整体引号包裹，Explorer 会解析失败并默认回退打开「我的文档」或「此电脑」。
    /// 此处将路径统一转为 Windows 原生反斜杠 `\`，并使用 `raw_arg` 传递 `/select,"<path>"`。
    pub(crate) fn reveal_path_in_explorer(path: &Path) {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            let mut cmd = std::process::Command::new("explorer");
            if path.is_file() {
                let norm = path.to_string_lossy().replace('/', "\\");
                cmd.raw_arg(format!("/select,\"{norm}\""));
            } else {
                let target_dir = if path.is_dir() {
                    path.to_path_buf()
                } else {
                    path.parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| crate::utils::AppConfig::resolve_path("."))
                };
                let _ = std::fs::create_dir_all(&target_dir);
                let norm = target_dir.to_string_lossy().replace('/', "\\");
                cmd.raw_arg(format!("\"{norm}\""));
            }
            let _ = cmd.spawn();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let target = if path.is_file() {
                path.parent().unwrap_or(path)
            } else {
                path
            };
            let _ = std::fs::create_dir_all(target);
            let _ = open::that(target);
        }
    }

    pub(crate) fn open_models_dir(&mut self, cx: &mut Context<Self>) {
        let dir = crate::utils::AppConfig::resolve_path("models");
        Self::reveal_path_in_explorer(&dir);
        cx.notify();
    }

    /// 在资源管理器里定位某个可下载组件的文件（找不到就退到它所在目录或软件安装根目录）。
    pub(crate) fn reveal_model_file(&mut self, item_id: &str, cx: &mut Context<Self>) {
        let Some(item) = crate::utils::model_download::item_by_id(item_id) else {
            self.state.download_status_msg = format!("未知组件：{item_id}");
            cx.notify();
            return;
        };
        let path = crate::utils::model_download::item_effective_path(item, &self.state.config);
        Self::reveal_path_in_explorer(&path);
        cx.notify();
    }

    /// 打开系统原生文件对话框选择**本地大模型**（离线 Qwen 用的 GGUF 文件）。
    ///
    /// 与 [`MainWindow::choose_file`] 同一套写法（`cx.spawn` + `rfd::AsyncFileDialog`）：
    /// 同步的 `FileDialog` 会起 Win32 模态循环，而 GPUI 在渲染回调里正持有 RefCell
    /// 借用，模态循环一旦重入就是借用冲突崩溃。异步版把选择过程交给系统，UI 线程
    /// 不被阻塞，选完再回主线程写配置。
    ///
    /// 为什么单独加这个入口：离线模型路径此前只能手改 `config.toml`（用户实测反馈），
    /// 而「我自己下了个 GGUF 想用」是最自然的诉求——让他手抄一条绝对路径既不友好、
    /// 也容易抄错。过滤器给出 `.gguf`，但**不锁死**：llama.cpp 只吃 GGUF，
    /// 让用户先看到文件、再由校验给出可操作提示，比直接藏掉非 gguf 文件更好排错。
    pub(crate) fn choose_llm_model_file(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择本地大模型（GGUF）")
                .add_filter("GGUF 模型", &["gguf"])
                .add_filter("所有文件", &["*"])
                .pick_file()
                .await;
            if let Some(file_handle) = handle {
                let path = file_handle.path().to_path_buf();
                let _ = this.update(cx, |this, cx| {
                    this.set_local_model_path(path.to_string_lossy().to_string(), cx);
                });
            }
        })
        .detach();
    }

    /// 接纳一个刚导入的媒体文件：立刻发布选中状态，再在后台探测时长。
    ///
    /// 「浏览本地文件」与「拖拽文件进窗口」共用这一条路径，避免两种入口
    /// 各写一套时长探测逻辑而产生行为漂移。
    pub(crate) fn adopt_media_file(&mut self, file: PathBuf, cx: &mut Context<Self>) {
        if !file.is_file() {
            self.state.status =
                ProcessStatus::Failed(format!("无法导入：{} 不是有效文件", file.display()));
            cx.notify();
            return;
        }
        info!("用户导入了转写文件: {:?}", file);
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        // 先把选中状态发布出去，让界面立刻有反馈；ffmpeg 探测放到后台执行器，
        // 避免阻塞这次实体更新闭包。
        self.state.transcribe_duration = 0.0;
        self.state.transcribe_file = Some(file.clone());
        let fname = file
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("媒体文件")
            .to_string();
        self.notice = Some(format!("已载入媒体文件：{fname}"));
        // 换了文件 → 「0 秒命中」的缓存结果必然作废。不清理的话，
        // 新文件会沿用上一个文件的缓存命中信息（界面显示错误的句数）。
        self.state.invalidate_cached_transcription();
        self.state.status = ProcessStatus::Idle;
        self.state.active_tab = WorkspaceTab::Generate;
        cx.notify();

        let duration_file = file.clone();
        cx.spawn(async move |this, cx| {
            let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
            let duration = cx
                .background_executor()
                .spawn(async move { ffmpeg.get_duration(&duration_file) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.state.transcribe_file.as_ref() == Some(&file) {
                    this.state.transcribe_duration = duration;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 导入一个**已有的字幕文件**（SRT / WebVTT / LRC / ASS(SSA) / TTML(EBU-TT-D /
    /// Netflix TTAL) / JSON / 纯文本）作为当前工程，跳过转写。
    ///
    /// # 为什么需要它
    ///
    /// 项目能写出八种字幕格式却一种都读不回来：手上有一份同事给的 SRT、或想接着改
    /// 上次的导出，此前只能把视频重新转写一遍。这个入口补上缺失的导入链路。
    ///
    /// # 几个刻意的取舍
    ///
    /// - **不自动匹配视频**：导入的字幕是否对得上某个视频，程序无从判断（时间轴可能
    ///   被剪过）。直接发布成当前工程，用户自己决定要不要再选视频。
    /// - **清空撤销栈**：与「转写完成」同一处理——上一支片子的快照留在栈里，Ctrl+Z
    ///   会把旧字幕灌回来并按新工程落库，属于数据损坏。
    /// - **TXT / LRC 的时间带估算成分**：TXT 整条时间轴都是 `reader` 按字数合成的；
    ///   LRC 只有每行的起点时间，中间各句用下一行起点补齐，**最后一句**的结束时间
    ///   同样按字数估算。两者都得明说，否则用户导出 SRT 时会以为时间是准的。
    /// - **不落库**：导入的工程没有对应的媒体记录，`active_task_id` 置 `None`，
    ///   用户要留存就自己导出（或后续接一个「另存为工程」）。
    #[allow(dead_code)]
    pub(crate) fn import_subtitle_file(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("导入字幕文件")
                .add_filter("字幕文件", &IMPORT_SUBTITLE_EXTS)
                .pick_file()
                .await;
            let Some(handle) = handle else { return };
            let path = handle.path().to_path_buf();
            // 落状态 / 清撤销栈 / 提示全部交给共用内核：这条路径与「把字幕拖进
            // 窗口」必须逐字一致，否则同一个文件两种入口会有两套收尾逻辑，
            // 迟早漂移（对话框多了个「重新编号」而拖拽少了，就是静默数据差异）。
            let _ = this.update(cx, |this, cx| this.import_subtitle_path(path, cx));
        })
        .detach();
    }

    /// 「给定一个路径，直接导入成当前工程」的内核——文件对话框与拖拽共用。
    ///
    /// 读盘放到后台执行器（大 JSON 字幕的解析不便宜），回到实体线程后交给
    /// [`MainWindow::apply_imported_subtitle`] 落状态。`queue_note` 是混合拖入时
    /// 「媒体已入队」的摘要：导入提示会把它拼在前面，免得导入提示把队列信息盖掉。
    pub(crate) fn import_subtitle_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.import_subtitle_path_with_note(path, None, cx);
    }

    /// [`MainWindow::import_subtitle_path`] 的带摘要版本（`queue_note` 见其说明）。
    pub(crate) fn import_subtitle_path_with_note(
        &mut self,
        path: PathBuf,
        queue_note: Option<String>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let read_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { crate::subtitle::reader::read_subtitle_file(&read_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(segs) => match this.apply_imported_subtitle(&path, segs) {
                        // 成功：导入提示拼在队列摘要后面，两条信息都不丢
                        Some(notice) => {
                            this.notice = Some(match queue_note {
                                Some(q) => format!("{q}；{notice}"),
                                None => notice,
                            });
                        }
                        // 字幕是空文件：红色状态已经写明原因，队列摘要仍要交代
                        None => this.notice = queue_note,
                    },
                    Err(e) => {
                        this.state.status = ProcessStatus::Failed(format!("导入字幕失败：{e}"));
                        this.notice = queue_note;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 把读好的字幕句灌进当前工程，返回给用户看的提示文案；`segs` 为空时置红色
    /// 状态并返回 `None`。
    ///
    /// 实际的状态改动在 [`install_imported_subtitles`] 里完成（同步、不 spawn、不
    /// 碰对话框）：两条导入入口只差「路径从哪来」，先落库再换文档、重新编号、清
    /// 撤销栈、清预览缓存、选首句、文案拼装必须逐字一致。这里只负责 `MainWindow`
    /// 自己的界面缓存失效。
    fn apply_imported_subtitle(&mut self, path: &Path, segs: Vec<Segment>) -> Option<String> {
        let notice = install_imported_subtitles(&mut self.state, path, segs)?;
        // 清单的「跟随选中」缓存记的是旧文档的行位置：整份文档换了必须让它失效，
        // 否则下一帧会按旧位置去定位新文档。
        self.subtitle_list_followed_sel = None;
        Some(notice)
    }

    /// 处理从操作系统拖入窗口的文件（GPUI 把外部文件拖放包装成一次内部 drag/drop）。
    ///
    /// 拖入多个**媒体**时全部进入批量队列 (F-012)：先入队并统一探测时长，再等用户
    /// 点「开始全部」。单个媒体文件仍走原有的「选中 + 探测时长」路径，保持单文件
    /// 交互手感不变。全部扩展名都不支持时给出明确提示，而不是静默无反应。
    ///
    /// # 字幕（P1-A11）
    ///
    /// 拖入的字幕文件此前被 `collect_media_files` 当成「非媒体文件」丢掉，用户完全
    /// 没有反馈。现在把拖入项分成两条清单：
    ///
    /// - **媒体**照旧交给 `media_scan` 递归展开（文件夹直接支持、去重、跳过原因
    ///   结构化返回）；
    /// - **字幕**只认「用户点名的那个文件」，不扫目录（理由见
    ///   `media_scan::collect_subtitle_files`：课程文件夹里的 `.json` 不是字幕，
    ///   把它自动导入会顶掉用户当前工程）。数量为 1 时走与「导入字幕」对话框**完全
    ///   相同**的内核；多于 1 个时明确说「一次只导入一个」，既不挑一个猜、也不静默。
    ///
    /// 媒体与字幕一起拖入时媒体照常入队、字幕照常导入，导入提示会带上前面的队列摘要。
    pub(crate) fn handle_dropped_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        // 拖进来的可能是**目录**（把一门课的文件夹整个拖进来是最自然的动作）。
        // 此前只按扩展名筛文件、目录被静默丢弃，用户拖了个文件夹进去看到的是
        // 「不是支持的音视频格式」——既没告诉他目录其实支持，也没说清一个文件都
        // 没找到。现在统一交给 `media_scan` 展开：目录递归、文件直收、去重、
        // 跳过原因结构化返回，好让这里的提示是有信息量的。
        //
        // 字幕单独收集一次（只看用户点名的文件，不递归目录）；两条清单互不干扰：
        // `.json` / `.srt` 不在媒体清单里，`.mp4` 也不会被当成字幕。
        let subtitles = crate::utils::media_scan::collect_subtitle_files(paths, &SUBTITLE_EXTS);
        let outcome = crate::utils::media_scan::collect_media_files(
            paths,
            &MEDIA_EXTS,
            crate::utils::media_scan::DEFAULT_MAX_DEPTH,
        );
        // 只借用 `files`，不把它 move 出去——后面 `outcome` 还要用来算提示摘要
        // （收了几个、跳过什么）。`files.len()` 先存下来，避免与借用打架。
        let found = outcome.files.len();

        // 「几号媒体、几号字幕走哪条路」的决策被拎成纯函数 `drop_route`，单测不用
        // 构造 `MainWindow` 就能把规则钉死；这里只负责执行它选中的分支。
        match drop_route(found, subtitles.len()) {
            DropRoute::Nothing => {
                self.state.status = ProcessStatus::Failed(self.drop_failure_message(&outcome));
                cx.notify();
            }
            DropRoute::SubtitleOnly => self.import_subtitle_path(subtitles[0].clone(), cx),
            DropRoute::TooManySubtitles => {
                self.notice = Some(too_many_subtitles_notice(subtitles.len()));
                cx.notify();
            }
            DropRoute::MediaOnly => self.route_media_only(paths, outcome, cx),
            DropRoute::MediaAndSubtitle => {
                let queue_note = self.enqueue_dropped_media(&outcome, cx);
                self.import_subtitle_path_with_note(subtitles[0].clone(), queue_note, cx);
            }
            DropRoute::MediaAndTooManySubtitles => {
                let queue_note = self.enqueue_dropped_media(&outcome, cx);
                let sub_note = too_many_subtitles_notice(subtitles.len());
                self.notice = Some(match queue_note {
                    Some(q) => format!("{q}；{sub_note}"),
                    None => sub_note,
                });
                cx.notify();
            }
        }
    }

    /// 这次拖入只有媒体时的既有分流：单个文件走「选中 + 探测时长」，从文件夹扫出来
    /// 的、以及多个文件全部入队。
    ///
    /// 单独抽出来只为让 [`MainWindow::handle_dropped_files`] 的分支一眼能看完；
    /// 内部逻辑与改动前逐字一致。
    fn route_media_only(
        &mut self,
        paths: &[PathBuf],
        outcome: crate::utils::media_scan::ScanOutcome,
        cx: &mut Context<Self>,
    ) {
        let media = &outcome.files;
        if media.len() == 1 {
            // 1. 若拖入的是文件夹，期待的是批量队列，始终入队
            if outcome.dirs_scanned > 0 || paths.iter().any(|p| p.is_dir()) {
                let added = self.state.enqueue_files(media.clone());
                self.state.transcribe_file = None;
                self.state.active_tab = WorkspaceTab::Generate;
                self.probe_queue_durations(cx);
                self.notice = Some(format!("已从文件夹加入 {added} 个文件到批量队列"));
                cx.notify();
                return;
            }
            // 2. 若当前批量队列已有条目：拖入单文件直接追加进队列，绝不覆盖冲毁队列
            if !self.state.batch_queue.is_empty() {
                let _added = self.state.enqueue_files(media.clone());
                self.state.transcribe_file = None;
                self.state.active_tab = WorkspaceTab::Generate;
                self.probe_queue_durations(cx);
                let name = media[0]
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("视频");
                self.notice = Some(format!(
                    "已将「{name}」追加到批量转写队列（共 {} 个）",
                    self.state.batch_queue.len()
                ));
                cx.notify();
                return;
            }
            // 3. 若当前已有选中的单文件：拖入第 2 个文件自动转为批量队列并合并两者
            if let Some(prev) = self.state.transcribe_file.take() {
                let added = self.state.enqueue_files(vec![prev, media[0].clone()]);
                self.state.transcribe_file = None;
                self.state.active_tab = WorkspaceTab::Generate;
                self.probe_queue_durations(cx);
                self.notice = Some(format!("已自动转为批量队列（共 {added} 个视频）"));
                cx.notify();
                return;
            }
            // 4. 空闲状态且仅拖入 1 个文件：单文件就绪
            self.adopt_media_file(media[0].clone(), cx);
            return;
        }

        // 多文件拖入：若先前有未转写的单文件，顺带合并入队
        if let Some(prev) = self.state.transcribe_file.take() {
            self.state.enqueue_files(vec![prev]);
        }
        let added = self.state.enqueue_files(media.clone());
        self.state.transcribe_file = None;
        if added == 0 {
            self.state.status =
                ProcessStatus::Failed("这些文件已经在批量队列里，未重复添加".to_string());
            cx.notify();
            return;
        }
        info!("批量队列新增 {} 个文件", added);
        self.state.active_tab = WorkspaceTab::Generate;
        self.probe_queue_durations(cx);
        self.notice = Some(Self::ingest_summary(added, &outcome));
        cx.notify();
    }

    /// 混合拖入（媒体 + 字幕）时把媒体部分入队，返回给用户看的入队摘要。
    ///
    /// 返回 `None` 表示一个都没新增（全都在队列里了）——此时不该报「已加入 0 个」，
    /// 让调用方的字幕提示单独说明即可。
    fn enqueue_dropped_media(
        &mut self,
        outcome: &crate::utils::media_scan::ScanOutcome,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        // 超大素材先过一道体检。几十 GB / 几十小时的素材会把抽帧缓存、波形、切块
        // 临时目录一起撑起来，而用户是在半小时后才看到「界面没反应」。所以：
        // 超过硬上限的**不入队**并说明原因，偏大的照常入队但把代价讲在前面。
        // 这里只用文件体积判断——拖入那一刻还没探时长，体积是最便宜的先手。
        let mut accepted: Vec<PathBuf> = Vec::with_capacity(outcome.files.len());
        let mut refused: Vec<String> = Vec::new();
        let mut warned: Option<String> = None;
        for path in &outcome.files {
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            match crate::utils::media_scan::assess_media_load(size, 0.0) {
                crate::utils::media_scan::MediaLoadAdvice::Ok => accepted.push(path.clone()),
                crate::utils::media_scan::MediaLoadAdvice::Warn(msg) => {
                    if warned.is_none() {
                        warned = Some(msg);
                    }
                    accepted.push(path.clone());
                }
                crate::utils::media_scan::MediaLoadAdvice::Refuse(msg) => {
                    refused.push(format!(
                        "{}（{msg}）",
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string())
                    ));
                }
            }
        }

        let added = self.state.enqueue_files(accepted);
        if added == 0 {
            // 一个都没入队且都被体积挡下：提示要直接给出原因，别退回「没找到文件」。
            if !refused.is_empty() {
                return Some(refused.join("；"));
            }
            return None;
        }
        info!("批量队列新增 {} 个文件", added);
        self.state.active_tab = WorkspaceTab::Generate;
        self.probe_queue_durations(cx);
        let summary = Self::ingest_summary(added, outcome);
        Some(match (refused.is_empty(), warned) {
            (true, None) => summary,
            (true, Some(w)) => format!("{summary}；{w}"),
            (false, w) => {
                let base = format!(
                    "{summary}；已跳过 {} 个超大文件：{}",
                    refused.len(),
                    refused.join("；")
                );
                match w {
                    Some(w) => format!("{base}；{w}"),
                    None => base,
                }
            }
        })
    }

    /// 一个文件都没收到时，把「为什么」拼成可操作的中文提示。
    ///
    /// 分三类说：① 完全没有可识别文件（附支持的扩展名）；② 有跳过项，逐类汇总次数
    /// 并给出**第一条**具体路径（全列出来会把状态栏挤爆，而用户通常只需要一个样例
    /// 就能定位问题）；③ 目录太深——这是唯一「改个设置就能救」的情形，单独点出来。
    fn drop_failure_message(&self, outcome: &crate::utils::media_scan::ScanOutcome) -> String {
        const SUPPORTED: &str = "mp4/mkv/mov/avi/flv/webm/mp3/wav/flac/m4a";
        // 字幕是拖拽新增的旁路：用户很可能正好拖了一份字幕却看到「没有音视频文件」，
        // 所以失败提示必须把「字幕也能拖、但一次一个」讲出来，并指明 txt 的入口。
        const SUBTITLE_HINT: &str =
            "字幕文件（srt/vtt/json/lrc/ass/ssa/ttml/dfxp/ttal/sbv）也可直接拖入，一次一个；纯文本 .txt 请用「导入字幕」对话框。";
        if outcome.skipped.is_empty() {
            return format!(
                "没有找到可处理的音视频文件（支持 {SUPPORTED}）。若拖入的是文件夹，请确认里面有这些格式的文件。{SUBTITLE_HINT}"
            );
        }
        // 按原因归类计数，并把每类的首个样例路径带上
        let mut by_reason: Vec<(crate::utils::media_scan::SkipReason, usize, PathBuf)> = Vec::new();
        for (path, reason) in &outcome.skipped {
            match by_reason.iter_mut().find(|(r, _, _)| r == reason) {
                Some((_, n, _)) => *n += 1,
                // SkipReason 未派生 Copy：clone 一份存进汇总（四个无载荷变体，
                // 克隆成本为零）。
                None => by_reason.push((reason.clone(), 1, path.clone())),
            }
        }
        let detail = by_reason
            .iter()
            .map(|(reason, n, sample)| {
                // `SkipReason::message` 取 `self`（四个无载荷变体，clone 零成本）；
                // 这里拿到的是引用，clone 一份再取文案。
                format!(
                    "{}（{n} 项，如 {}）",
                    reason.clone().message(),
                    sample.display()
                )
            })
            .collect::<Vec<_>>()
            .join("；");
        format!("没有扫描到可处理的音视频文件：{detail}。支持 {SUPPORTED}。{SUBTITLE_HINT}")
    }

    /// 成功导入多个文件时的一行摘要（收了几个、跳过了什么）。
    fn ingest_summary(added: usize, outcome: &crate::utils::media_scan::ScanOutcome) -> String {
        let dirs = if outcome.dirs_scanned > 0 {
            format!("（扫描了 {} 个文件夹）", outcome.dirs_scanned)
        } else {
            String::new()
        };
        if outcome.skipped.is_empty() {
            return format!("已加入 {added} 个文件到批量队列{dirs}");
        }
        // 只提「值得用户动手」的两类：太深（换个更具体的目录再拖）与不可读
        // （盘没挂 / 没权限）。**按枚举匹配，不按文案子串匹配**——后者会在
        // 文案改一个字之后静默失效（提示从此永远为空，且没有任何报错），
        // 这正是这一版要避免的那类「悄悄坏掉」。
        //
        // 「非媒体扩展名」不在这里报：拖一个课程文件夹进来，里面混着 .txt/.pdf
        // 是常态，逐条报只会把真正要紧的提示淹掉（失败分支才需要展开讲）。
        use crate::utils::media_scan::SkipReason;
        // 顺序即提示里出现的顺序：先讲「扫不动」的（太深），再讲「根本没看」的。
        let mut notable: Vec<String> = Vec::new();
        for reason in [
            SkipReason::TooManyFiles,
            SkipReason::TooDeep,
            SkipReason::Unreadable,
            SkipReason::Symlink,
        ] {
            let n = outcome.skipped.iter().filter(|(_, r)| *r == reason).count();
            if n > 0 {
                let what = match reason {
                    SkipReason::TooManyFiles => "次导入已达文件数上限，其余文件请分批导入",
                    SkipReason::TooDeep => "个文件夹层级过深未扫",
                    SkipReason::Unreadable => "个路径无法读取",
                    SkipReason::Symlink => "个快捷方式未跟随",
                    // 上面这个列表不含该类；真加了新变体进来会在编译期报错，
                    // 强迫作者顺手补一条文案——比运行时兜底更可靠。
                    SkipReason::UnsupportedExtension => "个非媒体文件已跳过",
                };
                notable.push(format!("{n} {what}"));
            }
        }
        if notable.is_empty() {
            format!("已加入 {added} 个文件到批量队列{dirs}")
        } else {
            format!(
                "已加入 {added} 个文件到批量队列{dirs}；{}",
                notable.join("、")
            )
        }
    }

    /// 批量选择文件加入队列（转写页「批量导入」按钮）
    pub(crate) fn choose_batch_files(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handles = rfd::AsyncFileDialog::new()
                .add_filter("音视频文件", &MEDIA_EXTS)
                .pick_files()
                .await;
            let Some(handles) = handles else { return };
            let files: Vec<PathBuf> = handles.iter().map(|h| h.path().to_path_buf()).collect();
            let _ = this.update(cx, |this, cx| {
                this.ingest_paths(files, cx);
            });
        })
        .detach();
    }

    /// 选择一个**文件夹**，把里面（含子目录）的媒体文件全部加入队列。
    ///
    /// 与「批量导入文件」分开两个按钮，是因为系统文件对话框一次只能是「选文件」或
    /// 「选文件夹」二选一，混在一起会让用户点开发现选不了目录而困惑。拖拽那条路
    /// 则两者都收（见 `handle_dropped_files`）。
    pub(crate) fn choose_batch_folder(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择包含音视频的文件夹")
                .pick_folder()
                .await;
            let Some(handle) = handle else { return };
            let dir = handle.path().to_path_buf();
            let _ = this.update(cx, |this, cx| {
                this.ingest_paths(vec![dir], cx);
            });
        })
        .detach();
    }

    /// 把一批「用户给的路径」（文件 / 目录混合）展开后入队，并给出有信息量的反馈。
    ///
    /// 「批量导入文件」「选择文件夹」两条入口共用它，因此「收了多少、跳过什么、
    /// 一个都没收到时为什么」这套提示只有一份实现，不会两处漂移。
    pub(crate) fn ingest_paths(&mut self, inputs: Vec<PathBuf>, cx: &mut Context<Self>) {
        let outcome = crate::utils::media_scan::collect_media_files(
            &inputs,
            &MEDIA_EXTS,
            crate::utils::media_scan::DEFAULT_MAX_DEPTH,
        );
        if outcome.files.is_empty() {
            self.state.status = ProcessStatus::Failed(self.drop_failure_message(&outcome));
            cx.notify();
            return;
        }
        let found = outcome.files.len();
        // clone 而不是 move：`outcome` 后面还要用来生成摘要（收了多少、跳过什么）。
        let added = self.state.enqueue_files(outcome.files.clone());
        if added == 0 {
            self.state.status =
                ProcessStatus::Failed(format!("这 {found} 个文件已经在批量队列里，未重复添加"));
        } else {
            info!("批量队列新增 {} 个文件", added);
            self.notice = Some(Self::ingest_summary(added, &outcome));
        }
        self.state.active_tab = WorkspaceTab::Generate;
        self.probe_queue_durations(cx);
        cx.notify();
    }

    /// 为队列中尚未探测时长的条目补齐时长（批量完成弹窗要按累计时长做统计）。
    ///
    /// 串行探测而不是并发：`ffmpeg -i` 对本地文件是毫秒级操作，并发派发只会让
    /// 进程表瞬间多出一堆短命进程，收益为零。
    pub(crate) fn probe_queue_durations(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<PathBuf> = self
            .state
            .batch_queue
            .iter()
            .filter(|item| item.duration <= 0.0)
            .map(|item| item.path.clone())
            .collect();
        if pending.is_empty() {
            return;
        }
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        cx.spawn(async move |this, cx| {
            for path in pending {
                let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path.clone());
                let probe_target = path.clone();
                let duration = cx
                    .background_executor()
                    .spawn(async move { ffmpeg.get_duration(&probe_target) })
                    .await;
                let probe_key = path.clone();
                let _ = this.update(cx, |this, cx| {
                    // 用路径而不是下标回写：探测期间用户可能删过队列条目，
                    // 下标会整体前移，按下标写会张冠李戴。
                    if let Some(idx) = this.state.queue_index_of(&probe_key) {
                        this.state.batch_queue[idx].duration = duration;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 从队列里移除一条（用户手动剔除不想要的片源）
    pub(crate) fn remove_queue_item(&mut self, idx: usize, cx: &mut Context<Self>) {
        let removed_path = self
            .state
            .batch_queue
            .get(idx)
            .map(|item| item.path.clone());
        if self.state.remove_queue_item(idx) {
            if let Some(path) = removed_path {
                if self.state.transcribe_file.as_ref() == Some(&path) {
                    if let Some(first) = self.state.batch_queue.first() {
                        self.state.transcribe_file = Some(first.path.clone());
                        self.state.selected_file = Some(first.path.clone());
                        self.state.transcribe_duration = first.duration;
                    } else {
                        self.state.transcribe_file = None;
                        self.state.selected_file = None;
                        self.state.transcribe_duration = 0.0;
                    }
                }
            }
            if self.state.batch_queue.is_empty() {
                self.state.transcribe_file = None;
                self.state.selected_file = None;
                self.state.transcribe_duration = 0.0;
            }
            cx.notify();
        }
    }

    /// 选中队列中的某一项并在中央工作台显示
    pub(crate) fn select_queue_item(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.state.batch_queue.len() {
            return;
        }
        self.state.batch_active = Some(idx);
        let path = self.state.batch_queue[idx].path.clone();
        let dur = self.state.batch_queue[idx].duration;
        self.state.transcribe_file = Some(path.clone());
        self.state.selected_file = Some(path);
        if dur > 0.0 {
            self.state.transcribe_duration = dur;
        }
        cx.notify();
    }

    /// 把队列里的某一条搬到指定下标（`from` → `to`），只动顺序。
    ///
    /// 刻意不碰正在进行的转写：重排不是「换一条跑」，正在跑的那条只是换了位置，
    /// 活动指针由 `AppState::move_queue_item` 按路径纠正，所以这里既不取消也不重启。
    pub(crate) fn move_queue_item(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if self.state.move_queue_item(from, to) {
            cx.notify();
        }
    }

    /// 清空队列并退出批量模式（已转写完成、已落库的工程不受影响）
    pub(crate) fn clear_batch_queue(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            self.request_cancel_processing(cx);
        }
        self.state.clear_batch_queue();
        self.state.transcribe_file = None;
        self.state.selected_file = None;
        self.state.transcribe_duration = 0.0;
        cx.notify();
    }

    /// 删除一条视频库记录，并把「当前工程被删掉了」这件事显式告诉用户。
    ///
    /// `AppState::delete_task_record` 在删掉的恰好是当前打开的工程时，会**静默**把
    /// 工作区换成列表里的下一条记录。用户回到剪辑台会发现字幕变成了另一个视频的，
    /// 却没有任何线索说明发生了什么。这里在删除后检查当前工程是否真的被换掉，
    /// 是则发一条**中性提示**（不是错误——删除本身成功了）说明切换到了哪一条。
    pub(crate) fn delete_task_record(&mut self, id: i64, cx: &mut Context<Self>) {
        // 连带清掉这个任务在界面侧的缓存。这两个 map 按 task_id 累积，
        // 而 `recent_tasks` 只保留最近 50 条——不清理的话，用户长时间使用
        // （不断导入/删除）后缓存会一直涨，且全是查不到的僵尸条目。
        // 注意必须放在删除**之前**：删除后 `recent_tasks` 里已找不到该 id，
        // 也就无从得知要清哪一条。
        self.library_thumbs.remove(&id);
        self.library_sizes.remove(&id);
        let was_current = self
            .state
            .recent_tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| {
                self.state.selected_file.as_ref().is_some_and(|p| {
                    p.to_string_lossy().replace('\\', "/") == t.file_path.replace('\\', "/")
                })
            })
            .unwrap_or(false);

        match self.state.delete_task_record(id) {
            Ok(()) => {
                self.notice = Some(if was_current {
                    match self
                        .state
                        .selected_file
                        .as_ref()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    {
                        Some(name) => format!("已删除当前工程，工作区已切换到「{name}」"),
                        None => "已删除当前工程，工作区已清空".to_string(),
                    }
                } else {
                    "已删除该记录".to_string()
                });
            }
            Err(err) => {
                // 删库失败：记录其实还在库里，不能提示「已删除」，也不能让工作区跟着
                // 切走。走跨页可见的失败横幅，别让用户以为删掉了。
                tracing::warn!(error = %err, "删除历史记录失败");
                self.notice = None;
                self.state.status = ProcessStatus::Failed(format!(
                    "删除记录失败：{err}（记录仍在视频库中，请检查磁盘空间或数据库是否被占用）"
                ));
            }
        }

        if self.state.selected_file.is_some() {
            self.trigger_extract_frame(cx);
        }
        cx.notify();
    }

    /// 「开始全部」：从第一个待处理项起，连续转写直到队列跑完。
    ///
    /// 失败项会被重新排队——用户点「开始全部」的语义就是「把没成功的再跑一遍」，
    /// 已经成功的条目则跳过，不重复烧算力。
    pub(crate) fn start_batch_queue(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        if self.state.batch_queue.is_empty() {
            self.state.status = ProcessStatus::Failed("批量队列是空的，先导入文件".to_string());
            cx.notify();
            return;
        }
        for item in self.state.batch_queue.iter_mut() {
            if item.state.is_failed() {
                item.state = QueueState::Pending;
            }
        }
        let Some(next) = self.state.queue_next_actionable() else {
            self.state.status =
                ProcessStatus::Failed("队列中的文件都已转写完成，无需重复处理".to_string());
            cx.notify();
            return;
        };
        self.state.batch_running = true;
        let path = self.state.batch_queue[next].path.clone();
        self.start_processing_for(path, cx);
    }

    /// 终止批量队列：停掉当前任务并停止续跑（已完成的条目保留结果）
    pub(crate) fn cancel_batch_queue(&mut self, cx: &mut Context<Self>) {
        self.state.batch_running = false;
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            self.request_cancel_processing(cx);
        } else {
            cx.notify();
        }
    }

    /// 取出队列里下一个待处理项并立即开跑；返回 `true` 表示已续上。
    ///
    /// 队列跑完时在这里收尾：复位批量模式并弹出批量汇总弹窗。
    fn advance_batch_queue(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.state.batch_running {
            return false;
        }
        match self.state.queue_next_actionable() {
            Some(next) => {
                let path = self.state.batch_queue[next].path.clone();
                self.start_processing_for(path, cx);
                true
            }
            None => {
                self.finish_batch_queue(cx);
                false
            }
        }
    }

    /// 批量队列收尾：统计结果并弹出汇总弹窗。
    ///
    /// 队列跑完后工作台停在上一条完成的工程上（`Finished` 分支已同步过），
    /// 这里只负责把「这一批到底成了几个、败了几个」讲清楚。
    fn finish_batch_queue(&mut self, cx: &mut Context<Self>) {
        self.state.batch_running = false;
        self.state.batch_active = None;
        let (done, failed, segments) = self.state.queue_summary();
        let total = self.state.batch_queue.len();
        let total_duration = self.state.queue_total_duration();
        let failed_names = self.state.queue_failed_names();
        info!("批量转写结束：{done} 成功 / {failed} 失败 / 共 {total} 个文件");
        self.state.status = ProcessStatus::Idle;
        // 工作台停在最后一条完成的工程上，波形在整个队列跑完后再抽一次即可
        self.ensure_waveform(cx);
        self.completion_dialog = Some(CompletionDialogInfo {
            file_name: format!("本次共处理 {total} 个文件"),
            segment_count: segments,
            total_duration,
            metrics: None,
            batch: Some(BatchSummary {
                total,
                done,
                failed,
                segments,
                total_duration,
                failed_names,
            }),
        });
        cx.notify();
    }

    /// 智能缓存命中处理：直接 0 秒载入已缓存的解析结果
    ///
    /// 入参是 `Arc<TaskRecord>`：调用点在渲染路径上拿到的是缓存的共享引用，
    /// 这里按需 `(*cached).clone()` 展开一次即可——载入是**一次性**动作，
    /// 不像渲染那样每帧发生，复制一次整份字幕可接受。
    pub(crate) fn load_cached_result(
        &mut self,
        cx: &mut Context<Self>,
        cached: std::sync::Arc<crate::storage::TaskRecord>,
    ) {
        let filename = cached.file_name.clone();
        let seg_count = cached.segments.len();
        let total_dur = cached.duration;
        let metrics = cached.metrics.clone();

        info!("智能缓存命中: 0 秒载入 {:?}", filename);
        self.state.load_from_cache((*cached).clone());
        self.state.active_tab = crate::app::state::WorkspaceTab::Editor;
        self.ensure_waveform(cx);

        self.completion_dialog = Some(CompletionDialogInfo {
            file_name: filename,
            segment_count: seg_count,
            total_duration: total_dur,
            metrics,
            batch: None,
        });
        cx.notify();
    }

    /// 触发流水线处理（单文件入口：跑完弹单文件完成框）
    pub(crate) fn start_processing(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.state.transcribe_file.clone() else {
            return;
        };
        // 单文件入口显式退出批量续跑语义：否则这一条跑完会把整个队列也顺手带跑，
        // 用户点「开始转写」时并没有这个预期。
        self.state.batch_running = false;
        self.state.batch_active = None;
        self.start_processing_for(file, cx);
    }

    /// 当前选中的识别档位缺什么就返回一句可操作的中文原因；齐备则 `None`。
    ///
    /// 抽成函数是为了让「开工守卫」与将来的其它入口（例如快捷键、命令面板）
    /// 复用同一份判定，而不是各自拼字符串——那类复制粘贴正是「文案一处改、另一处
    /// 忘了改」的来源。
    pub(crate) fn missing_engine_reason(&self) -> Option<String> {
        use crate::app::WhisperModelTier;
        let tier = self.state.whisper_model_tier;
        if tier == WhisperModelTier::SenseVoice {
            let model_ok = self.state.model_is_present("sensevoice-model")
                || crate::utils::AppConfig::resolve_path("models/sensevoice/model.int8.onnx")
                    .is_file();
            let tokens_ok = self.state.model_is_present("sensevoice-tokens")
                || crate::utils::AppConfig::resolve_path("models/sensevoice/tokens.txt").is_file();
            let vad_ok = self.state.model_is_present("sensevoice-vad")
                || crate::utils::AppConfig::resolve_path("models/sensevoice/silero_vad.onnx")
                    .is_file();
            let runner_ok =
                crate::utils::AppConfig::resolve_path("tools/sensevoice_runner.py").is_file();

            if !model_ok || !tokens_ok || !vad_ok {
                return Some(
                    "SenseVoice 组件未就齐（需模型、词表与 VAD）：请在「性能设置 → 模型与组件」一键下载补齐，或在右侧切换为 Whisper 档位"
                        .to_string(),
                );
            }
            if !runner_ok {
                return Some(
                    "SenseVoice 运行脚本缺失 (tools/sensevoice_runner.py)，请检查安装包完整性"
                        .to_string(),
                );
            }
            return None;
        }
        // Whisper 各档：判据取**管线真正会加载的那个文件**（`model_relative_path`
        // 已含「首选量化缺失时平滑回退」的逻辑），而不是 `model_is_present`。
        // 两者在正常情形下同结论，但用户把 `paths.whisper_model` 指到一个**非档位
        // 命名**的自备模型时，`model_is_present` 会因「名字对不上任何档位」判为缺失，
        // 而管线其实跑得起来——用实际路径判定就不会误拦。
        // 判据取**当前选中的那个文件**（`selected_model_path`），而不是
        // 「档位 → 内置路径」——后者会让用户自备的模型（配置里指向任意文件）
        // 被误判为「内置档位没下」，明明能跑却被拦下。
        let will_load = self.state.selected_model_path.clone();
        if !will_load.is_file() {
            return Some(format!(
                "{} 模型未就位：请在「性能设置 → 模型与组件」下载，或在右侧「识别引擎」卡里改选一个已就位的模型",
                self.state.selected_model_label()
            ));
        }

        let configured_cli =
            crate::utils::AppConfig::resolve_path(&self.state.config.paths.whisper_cli);
        let has_cli = configured_cli.is_file()
            || crate::utils::AppConfig::resolve_path("tools/whisper-cli.exe").is_file()
            || crate::utils::AppConfig::resolve_path(
                "tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe",
            )
            .is_file()
            || crate::utils::AppConfig::resolve_path("tools/whisper-vulkan/whisper-cli.exe")
                .is_file()
            || crate::utils::AppConfig::resolve_path("tools/whisper-cuda/whisper-cli.exe")
                .is_file();

        if !has_cli {
            let sv_ready = self.state.model_is_present("sensevoice-model");
            return Some(if sv_ready {
                "Whisper 推理程序未就位：请到「模型与组件」下载「whisper.cpp 识别程序」（约4MB），或在右侧切换为已就绪的「SenseVoice 极速」引擎".to_string()
            } else {
                "Whisper 推理程序未就位：请到「性能设置 → 模型与组件」下载「whisper.cpp 识别程序」（约4MB）".to_string()
            });
        }

        None
    }

    /// 启动一次转写，显式指定输入文件。
    ///
    /// 单文件入口与批量续跑共用这一条启动路径：差别只在「跑完之后做什么」，
    /// 启动前的准备（停预览、拼运行时参数、派发后台线程）完全一致。
    pub(crate) fn start_processing_for(&mut self, file: PathBuf, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        // 开工前先把当前工程的未落库编辑写回：新一轮转写会在管线回报时整体替换
        // `segments`，落库又是 1.2s 去抖的——用户「改完字幕就点了另一个文件的开始」
        // 会丢掉最后一段编辑。写失败时拦下这次开工（横幅说明原因），与关窗守卫同一取舍。
        if !self.flush_segments_before_close(cx) {
            return;
        }
        // 开工前先确认**选中的档位**确实有模型。
        //
        // 档位下拉现在会明示「已就位 / 未下载」并就地给下载按钮，用户完全可以先选中
        // 一档再决定要不要下。若这里不拦，`model_override` 会指向一个不存在的文件，
        // 报错来自 whisper-cli 的 stderr（「failed to load model」之类），
        // 用户看不出是「档位没下」还是「文件坏了」。这里提前给可操作的中文提示，
        // 并把他送回能下载的那个界面。
        //
        // 只在**单文件入口**拦：批量续跑时若中途某档位文件被删，报错也应落到那一条
        // 任务上，而不是整批静默停住——不过批量与单文件共用本函数，所以这条守卫对
        // 两者一致：都停在开工前，都不会把任务跑成半截。
        if let Some(reason) = self.missing_engine_reason() {
            self.state.status = ProcessStatus::Failed(reason);
            cx.notify();
            return;
        }
        // Preview decoding is CPU-heavy on a CPU-only machine. Stop it before
        // Whisper starts so the ASR workers get the available cores and memory
        // bandwidth instead of competing with 25 fps BGRA conversion.
        if self.state.is_playing {
            self.state.current_time = self.state.video_player.current_play_time();
        }
        self.state.video_player.stop();
        self.state.is_playing = false;
        self.play_tick_generation = self.state.video_player.generation();

        // 批量模式：把这一条标记为「转写中」，并把入队时探测好的时长喂给进度条。
        // 时长必须在这里回填——管线只上报进度，不上报总时长。
        if self.state.batch_running {
            if let Some(idx) = self.state.queue_index_of(&file) {
                self.state.batch_active = Some(idx);
                self.state.mark_queue_running(idx);
                let probed = self.state.batch_queue[idx].duration;
                if probed > 0.0 {
                    self.state.transcribe_duration = probed;
                }
            }
        }

        let input_file = file;
        self.state.transcribe_file = Some(input_file.clone());

        let (tx, mut rx) = mpsc::unbounded_channel();
        self.state.clear_streaming();
        self.state.cancel_requested = false;
        // 尽早在 UI 线程作废上一轮的取消状态（管线侧 + 引擎侧）。**必须留在 UI 线程、
        // 且早于下面 spawn 任务线程**：把它挪进任务线程就是原本那条「终止转写失效」
        // 的竞态——线程刚起来、还没跑到复位，用户已点「终止」置了位，随后被任务自己
        // 清零，转写照跑完。放在这里之后，任务线程里不再有任何清标志的写方，派发口
        // 又由函数开头的 `status != Processing` 守卫兜底，上一轮的取消不会误伤新任务。
        self.state.pipeline.begin_task();
        self.state.transcribe_started_at = Some(std::time::Instant::now());
        self.state.status = ProcessStatus::Processing {
            stage: "准备中...".to_string(),
            progress: 0.0,
            detail: "正在启动异步任务管线".to_string(),
        };
        cx.notify();

        let pipeline = self.state.pipeline.clone();
        let lang = if self.state.language == "auto" {
            None
        } else {
            Some(self.state.language.clone())
        };
        let fmt = self.state.output_format.clone();
        let polish = self.state.enable_polish;
        let polish_mode = Some(self.state.polish_mode.as_str().to_string());
        let threads = Some(self.state.whisper_threads);

        // 根据档位计算运行时模型路径覆盖（绝对路径）
        // 运行时模型路径 = 用户选中的那个文件。这里**必须**是 selected_model_path
        // 而不是「档位 → 内置路径」：后者会让 config.toml 里指定的自备模型永远
        // 不被加载（README 承诺「文件名可在 config.toml 中修改」），而且界面与
        // 命令行会各自跑不同的模型。
        let model_override = Some(self.state.selected_model_path.clone());
        let rescue_logprob = self.state.config.pipeline.whisper_rescue_logprob;
        // 音频前端预处理：把配置里的开关映射成管线参数。默认全开——
        // 它改的是「喂给模型的音频量」，是唯一还有量级空间的提速方向。
        let preprocess = crate::core::pipeline::PreprocessOptions {
            enabled: self.state.config.pipeline.preprocess_enabled,
            filters: crate::engines::SpeechFilterOptions {
                highpass_hz: self.state.config.pipeline.preprocess_highpass_hz,
                denoise: self.state.config.pipeline.preprocess_denoise,
                normalize: self.state.config.pipeline.preprocess_normalize,
                target_rate: crate::engines::ASR_SAMPLE_RATE,
            },
            compaction: crate::engines::CompactionConfig::default(),
            compact: self.state.config.pipeline.preprocess_compact,
            min_saving: self.state.config.pipeline.preprocess_min_saving,
        };
        let whisper_options = crate::core::pipeline::WhisperRuntimeOptions {
            audio_speed: self.state.config.pipeline.whisper_audio_speed,
            vad_threshold: self.state.config.pipeline.whisper_vad_threshold,
            preprocess,
        };
        // 说话人分离（F-015）：开关与期望人数都取自配置，关时管线完全跳过该阶段
        let diarization = crate::core::pipeline::DiarizationOptions {
            enabled: self.state.config.pipeline.enable_diarization,
            speakers: self.state.config.pipeline.speaker_count,
        };

        // 后台通过独立线程执行 Tokio 异步管线，不阻塞 UI 主线程
        let in_file = input_file.clone();
        std::thread::spawn(move || {
            // 运行时创建失败不能在后台线程里 panic：那会让线程带着未发送的终态
            // 事件消失，界面卡在「转写中」。显式发一条 Error 事件，走与管线报错
            // 相同的收尾路径（清状态、批量续跑）。
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = tx.send(PipelineEvent::Error(format!("转写运行时初始化失败：{err}")));
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(err) = pipeline
                    .run_with_options(
                        in_file,
                        None,
                        lang,
                        fmt,
                        polish,
                        polish_mode,
                        threads,
                        model_override,
                        rescue_logprob,
                        whisper_options,
                        diarization,
                        tx.clone(),
                    )
                    .await
                {
                    let _ = tx.send(PipelineEvent::Error(format!("转写失败：{err}")));
                }
            });
        });

        // 使用 GPUI 官方实体协程封装，避免复用 AsyncApp 引用造成重入借用冲突。
        cx.spawn(async move |this, cx| {
            loop {
                let Some(first_event) = rx.recv().await else {
                    // 通道关闭：发送端（后台线程）已析构。正常跑完时 Finished/Error
                    // 分支已把状态收回 Idle，这里无事可做；若状态仍停在 Processing，
                    // 说明线程异常退出（panic 等）——必须在这里兜底收尾，否则界面
                    // 永远显示「转写中」，且 `start_processing_for` 的 Processing
                    // 守卫会让用户再也开不了下一单。
                    let _ = this.update(cx, |this, cx| {
                        if settle_aborted_transcription(&mut this.state) {
                            this.advance_batch_queue(cx);
                        }
                        cx.notify();
                    });
                    break;
                };
                // Whisper 会连续产生大量事件，先聚合一个短窗口，保证 GPUI
                // 每帧只执行一次实体更新，避免高频重入 App RefCell。
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(80))
                    .await;
                let mut events = vec![first_event];
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }

                // Finished/Error 是管线的终态事件：处理完就退出事件循环，不再等
                // 通道关闭——终态之后不该再有任何事件改变界面状态。
                let mut terminal = false;
                let _ = this.update(cx, |this, cx| {
                    for event in events {
                        match event {
                            PipelineEvent::StageChanged(stage) => {
                                if let ProcessStatus::Processing {
                                    stage: ref mut s, ..
                                } = &mut this.state.status
                                {
                                    *s = stage;
                                }
                            }
                            PipelineEvent::Progress {
                                stage,
                                progress,
                                detail,
                            } => {
                                // 先把本条队列进度落进当前活动条目，再更新全局状态：
                                // 两者用的是同一个事件，但队列卡片要的是「这一条的
                                // 百分比」，而全局状态只服务单文件进度条/抽屉。
                                this.state.record_batch_progress(&stage, progress);
                                this.state.status = ProcessStatus::Processing {
                                    stage,
                                    progress,
                                    detail,
                                };
                            }
                            PipelineEvent::SegmentStream(seg) => {
                                this.state.push_stream_segment(seg);
                            }
                            PipelineEvent::Finished(segments, metrics) => {
                                this.state.clear_streaming();
                                this.state.transcribe_started_at = None;
                                if this.state.cancel_requested {
                                    // 用户主动终止：静默收尾，不弹完成框、不写历史库
                                    this.state.cancel_requested = false;
                                    this.state.status = ProcessStatus::Idle;
                                    this.state.transcribe_file = None;
                                    this.state.transcribe_duration = 0.0;
                                    // 批量模式下终止：当前项记为取消，并停掉续跑。
                                    // 已经跑完的条目保留结果，不清零。
                                    this.state
                                        .finish_active_queue_item(Err("已取消".to_string()));
                                    this.state.batch_running = false;
                                } else if segments.is_empty() {
                                    this.state.status = ProcessStatus::Failed(
                                        "未能识别出任何有效字幕，请检查音频音量或识别语言设置"
                                            .to_string(),
                                    );
                                    this.state.finish_active_queue_item(Err(
                                        "未识别出有效字幕".to_string()
                                    ));
                                } else {
                                    let finished_file = this.state.transcribe_file.take();
                                    let file_path =
                                        finished_file.or_else(|| this.state.selected_file.clone());
                                    let filename = file_path
                                        .as_ref()
                                        .and_then(|p| p.file_name())
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("media.mp4")
                                        .to_string();
                                    let total_dur = if this.state.transcribe_duration > 0.0 {
                                        this.state.transcribe_duration
                                    } else if this.state.total_duration > 0.0 {
                                        this.state.total_duration
                                    } else {
                                        segments.last().map(|s| s.end).unwrap_or(0.0)
                                    };
                                    let seg_count = segments.len();
                                    // 用 batch_running 而不是 batch_active 判断：用户删掉队列里
                                    // 正在跑的条目时 batch_active 会被清空，但这一批仍在续跑。
                                    // 若按 batch_active 判断，就会在批量中途弹出单文件完成框
                                    // 并逐条重抽波形。
                                    let in_batch = this.state.batch_running;

                                    // 1~2. 三件重活全部丢给后台：历史库写入（整份字幕 JSON 序列化 +
                                    //      视频首尾采样指纹读盘）、最近任务列表刷新、上一个工程的
                                    //      脏字幕落库。改前它们在渲染线程上一口气跑完，进度条刚
                                    //      走到 100% 界面就僵住 0.3~1s，用户以为卡死。现在 UI
                                    //      状态照旧立刻更新，只有「写库 / 读库 / 序列化」往后放。
                                    //
                                    //      后台任务内部顺序与改前逐条一致（刻意保证的语义）：
                                    //      ① insert_task → ② 刷新最近任务 → ③ 用**旧**的
                                    //      active_task_id 写回脏字幕。提示条的挂载顺序同理：
                                    //      ③ 的结局先落地（成功会清空 db_write_error），① 的错误
                                    //      最后补挂——反过来会被 flush 的成功顺手吞掉。
                                    let old_task_id = this.state.active_task_id;
                                    // ③ 的输入必须在替换 `segments` 之前抓下来：后台任务拿不到
                                    // 即将被换掉的旧数组；脏标记也在这里就清掉，否则回写期间
                                    // 用户任何一次落库都会把**新**工程的 segments 写进**旧**
                                    // 工程的 id（静默数据损坏）。
                                    // 序号与闸门都在派发这一刻取：收尾写跑在后台线程，
                                    // 不能自己领号，带上这个待发号去过闸即可（水位判据
                                    // 见 `AppState::next_write_seq`）。
                                    let flush_gate = this.state.segments_write_gate().clone();
                                    let flush_seq = this.state.next_write_seq();
                                    let flush_payload = if this.state.segments_dirty {
                                        this.state.segments_dirty = false;
                                        Some(DirtySegmentFlush {
                                            task_id: old_task_id,
                                            segments: this.state.segments.clone(),
                                            seq: flush_seq,
                                        })
                                    } else {
                                        None
                                    };
                                    // ① 的输入同理：`segments` 马上要交给状态，而 insert_task
                                    // 只有后台任务用得到，提前克隆一份带过去。
                                    let insert_payload = file_path.as_ref().map(|path| HistoryInsert {
                                        file_path: path.to_string_lossy().to_string(),
                                        file_name: filename.clone(),
                                        duration: total_dur,
                                        segments: segments.clone(),
                                    });
                                    let metrics_for_db = metrics.clone();
                                    let db = this.state.db.clone();
                                    // 派发时提示条上已有的写库错误（用户自己那次落库刚失败过）：
                                    // 回写时只有它没被人改过，才允许按本次 flush 的结局清空它。
                                    let db_error_at_spawn = this.state.db_write_error.clone();

                                    // 2. 将本次成果同步至剪辑校对工作台。
                                    //    覆盖前先把上一个工程的未保存改动落库（批量连跑时用户
                                    //    可能在上一支片子上改过字幕，不能默默丢掉）——这件事
                                    //    已随 ③ 一起进了后台队列。
                                    //    注意顺序：③ 必须用**旧**的 active_task_id 写回，所以
                                    //    新 id 要等这份结果在回写时确认仍属于当前文档才登记。
                                    //    写库失败不能静默：否则这条转写结果不会出现在历史库里，
                                    //    后续「保存字幕」也会因 active_task_id 为空而无处可写。
                                    if let Some(path) = file_path.as_ref() {
                                        this.state.selected_file = Some(path.clone());
                                        this.state.preview_source = Some(path.clone());
                                    }
                                    this.state.segments = segments;
                                    // 新转写完成：把「翻译语言」对齐到这份字幕识别出来的语言。
                                    // 目标语言是长期偏好，但换了视频不改，就会拿上一个视频的
                                    // 目标语言去译这一个（日语视频配残留的「日本語」= 日语译日语）。
                                    this.state.adopt_source_language_for_translation();
                                    this.state.bump_doc_epoch();
                                    // 转写完成后套用错别字记忆（用户以前改过的同音错字）。
                                    if this.state.apply_typo_memory() > 0 {
                                        info!("已应用错别字记忆库纠正");
                                    }
                                    // 登记 `active_task_id` 要用的 id 此刻还在后台，先记下这份
                                    // 结果的「文档身份」：回写时若对不上，说明用户已经点开了别的
                                    // 工程、或又开了一单转写，那就绝不能把这份 id 挂到当前的
                                    // segments 上——后续「保存字幕」会写错行（静默数据损坏）。
                                    let identity = DocumentIdentity {
                                        active_task_id: old_task_id,
                                        selected_file: this.state.selected_file.clone(),
                                    };
                                    // 新一轮转写的结果替换了整份文档：撤销栈里还是上一支
                                    // 片子的快照，留着的话 Ctrl+Z 会把上一支的字幕灌进这一支，
                                    // 并按新的 active_task_id 落库覆盖刚写好的记录。
                                    this.state.reset_edit_history();
                                    this.state.bump_segments_revision();
                                    if let Some(first) = this.state.segments.first() {
                                        this.state.select_segment(first.index);
                                    }
                                    this.state.total_duration = total_dur;

                                    // 3. 语音转写界面彻底解耦复位（等待下一次导入）
                                    this.state.status = ProcessStatus::Idle;
                                    this.state.transcribe_file = None;
                                    this.state.transcribe_duration = 0.0;

                                    // 4. 后台收尾的回写：写库结果按「最近任务 → 缓存失效 → 提示条
                                    //    → 身份校验后登记 active_task_id」的顺序落地，与改前的
                                    //    同步版逐条对应。
                                    cx.spawn(async move |this, cx| {
                                        let outcome = cx
                                            .background_executor()
                                            .spawn(async move {
                                                finish_db_work(
                                                    &db,
                                                    &flush_gate,
                                                    insert_payload,
                                                    Some(&metrics_for_db),
                                                    flush_payload,
                                                )
                                            })
                                            .await;
                                        let _ = this.update(cx, |this, cx| {
                                            // ② 最近任务列表：与改前一样只在查询成功时覆盖。
                                            if let Some(tasks) = outcome.recent_tasks {
                                                this.state.recent_tasks = tasks;
                                            }
                                            // 刚写入了新记录 → 让「0 秒命中」缓存失效，否则
                                            // 再次选中同一文件会显示旧的命中状态（改前这一步
                                            // 也只在有文件路径时做）。
                                            if outcome.wrote_history {
                                                this.state.invalidate_cached_transcription();
                                            }
                                            // ③ 的结局先落地（成功清空 db_write_error），① 的
                                            // 错误最后补挂：反过来会被 flush 的成功顺手吞掉
                                            // （改前注释解释过这一点）。
                                            let applied_flush = flush_outcome_to_apply(
                                                db_error_at_spawn.as_deref(),
                                                this.state.db_write_error.as_deref(),
                                                &outcome.flush,
                                            );
                                            this.state.db_write_error = finish_db_write_error(
                                                this.state.db_write_error.take(),
                                                &applied_flush,
                                                outcome.insert_error,
                                            );
                                            // 主键与脏标记只在「这份结果仍属于当前文档」时登记：
                                            // 后台这段时间里用户可能已经点开别的工程、或又开了
                                            // 一单转写（判据与理由见 `still_same_document`）。
                                            let current = DocumentIdentity {
                                                active_task_id: this.state.active_task_id,
                                                selected_file: this.state.selected_file.clone(),
                                            };
                                            if still_same_document(&identity, &current) {
                                                if matches!(
                                                    outcome.flush,
                                                    SegmentFlushOutcome::Failed(_)
                                                ) {
                                                    // 与改前 flush 的失败语义一致：写失败要把
                                                    // 脏标记置回，后续 flush 会重试，关窗守卫
                                                    // 也仍会拦。
                                                    this.state.segments_dirty = true;
                                                }
                                                this.state.active_task_id = outcome.new_task_id;
                                            } else {
                                                tracing::warn!(
                                                    "收尾写库回写时当前文档已切换，跳过 active_task_id 登记（避免后续保存写到别的工程记录上）"
                                                );
                                            }
                                            cx.notify();
                                        });
                                    })
                                    .detach();

                                    if in_batch {
                                        // 批量模式：只结算这一条。波形提取与完成弹窗都留到
                                        // 队列跑完再一次性处理，否则每完成一条都要重抽一次
                                        // 波形，纯属白烧解码。
                                        this.state.finish_active_queue_item(Ok(seg_count));
                                    } else {
                                        // 单文件入口跑的可能正是**队列里的某一条**：用户点了
                                        // 队列卡片（select_queue_item 会把这个文件设为
                                        // transcribe_file）再按「开始转写」，走的就是这条路。
                                        //
                                        // 这里必须补一次结算：in_batch 为假意味着
                                        // finish_active_queue_item 不会被调用，那条队列项就会
                                        // 永远停在「等待中」；而入队时已按 pending 落盘，
                                        // 于是每次重启它都再冒出来——用户看到的现象是
                                        // 「明明转写完了，队列里总挂着一条待转写」。
                                        if let Some(done_path) = file_path.as_deref() {
                                            this.state
                                                .settle_queue_item_by_path(done_path, seg_count);
                                        }
                                        this.ensure_waveform(cx);

                                        // 5. 弹出全屏转写完成提醒弹窗 (包含性能基准看板)
                                        this.completion_dialog = Some(CompletionDialogInfo {
                                            file_name: filename,
                                            segment_count: seg_count,
                                            total_duration: total_dur,
                                            metrics: Some(metrics),
                                            batch: None,
                                        });
                                    }
                                }
                                // 批量续跑：队列没跑完就立刻取下一条，跑完则弹批量汇总
                                if this.state.batch_running {
                                    this.advance_batch_queue(cx);
                                }
                                terminal = true;
                            }
                            PipelineEvent::Error(err) => {
                                if this.state.cancel_requested {
                                    // 终止导致的子进程报错按取消处理，不显示为失败
                                    this.state.cancel_requested = false;
                                    this.state.status = ProcessStatus::Idle;
                                    this.state.transcribe_file = None;
                                    this.state.transcribe_duration = 0.0;
                                    this.state
                                        .finish_active_queue_item(Err("已取消".to_string()));
                                    this.state.batch_running = false;
                                } else if this.state.batch_running {
                                    // 批量模式：单个文件出错不该拖垮整批。失败原因记在队列卡片上，
                                    // 立刻续跑下一个，最后在汇总弹窗里一并点名。
                                    // 这里同样按 batch_running 判断，理由见 Finished 分支。
                                    this.state.transcribe_file = None;
                                    this.state.transcribe_duration = 0.0;
                                    this.state.finish_active_queue_item(Err(err));
                                } else {
                                    this.state.status = ProcessStatus::Failed(err);
                                }
                                if this.state.batch_running {
                                    this.advance_batch_queue(cx);
                                }
                                terminal = true;
                            }
                        }
                    }
                    cx.notify();
                });

                if terminal {
                    break;
                }
            }
        })
        .detach();
    }

    /// 通用导出流程：编辑落库 -> 弹系统保存对话框 -> 后台写出文件 -> 状态回写。
    /// 统一四个导出入口的重复骨架（对话框参数与写出函数因格式而异）。
    pub(crate) fn export_with_save_dialog(
        &mut self,
        cx: &mut Context<Self>,
        default_name: String,
        filter_label: &str,
        filter_ext: String,
        write_file: impl FnOnce(&[crate::subtitle::Segment], &std::path::Path) -> anyhow::Result<()>
            + Send
            + 'static,
    ) {
        if self.state.segments.is_empty() {
            return;
        }
        self.state.flush_segments_if_dirty();

        let segments = self.state.segments.clone();
        let filter_label = filter_label.to_string();
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_file_name(&default_name)
                .add_filter(&filter_label, &[filter_ext.as_str()])
                .save_file()
                .await;
            if let Some(save_handle) = handle {
                let save_path = save_handle.path().to_path_buf();
                let res = write_file(&segments, &save_path);
                let _ = this.update(cx, |this, cx| {
                    match res {
                        Ok(()) => {
                            info!("字幕成功导出至: {:?}", save_path);
                            // 成功必须给出反馈：此前这里只有一行日志，界面上毫无动静。
                            // 用户点了「导出」、选了路径、然后**什么都没有发生**——
                            // 无法区分「成功」与「没点到」，只能去文件管理器里翻。
                            // 走中性提示条（notice）而不是错误条：这是成功操作。
                            this.notice = Some(format!(
                                "已导出 {} 句字幕到 {}",
                                segments.len(),
                                save_path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| save_path.display().to_string())
                            ));
                        }
                        Err(e) => {
                            // 导出失败也要走跨页可见的提示：`ProcessStatus::Failed` 只有
                            // 转写页的底部状态栏会渲染，而导出是在剪辑台做的。
                            this.state.status = ProcessStatus::Failed(format!("导出失败：{}", e));
                            this.notice = None;
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 剪映专业版草稿一键直接注入（自动写入本机剪映草稿库，打开剪映即可见）
    pub(crate) fn export_jianying_local(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        self.state.flush_segments_if_dirty();

        let segments = self.state.segments.clone();
        // 条数先取出来：`segments` 会被 move 进后台任务，成功提示还要用它
        let seg_count = segments.len();
        // 工程文件按「导出模式」（原文 / 仅译文 / 双语）写入；有译文时压成单行。
        let mode = self.editor_export_mode;
        let video_path = self.state.selected_file.clone();
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Voice2Word")
            .to_string();

        // 媒体时长：剪映草稿的视频轨必须用**真实媒体时长**，不能用最后一句字幕的
        // end（字幕只覆盖前 20 分钟时，草稿会把素材裁短）。
        let media_duration = self.state.total_duration;
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let media = crate::subtitle::jianying::DraftMedia {
                        path: video_path.clone(),
                        duration_secs: (media_duration > 0.0).then_some(media_duration),
                    };
                    crate::subtitle::JianYingExporter::inject_to_local_jianying_with_media(
                        &segments, &media, &stem, mode,
                    )
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(draft_path) => {
                        info!("成功注入剪映草稿: {:?}", draft_path);
                        let _ = std::process::Command::new("explorer")
                            .arg(&draft_path)
                            .spawn();
                        this.state.status = ProcessStatus::Idle;
                        // 已经打开了资源管理器，但仍给一条提示说明「注入了什么」：
                        // 只弹窗不说明的话，用户不知道这是导入成功还是碰巧打开了目录。
                        this.notice = Some(format!(
                            "已注入剪映草稿（{seg_count} 句字幕），打开剪映即可在首页看到"
                        ));
                    }
                    Err(e) => {
                        tracing::error!("剪映草稿注入失败: {:?}", e);
                        this.state.status =
                            ProcessStatus::Failed(format!("剪映草稿注入失败：{}", e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 内嵌字幕导出：把「原文 + 各语言译文」作为**软字幕轨**封装进 mkv。
    ///
    /// 交付/上传时需要的是「一个能直接播、能切语言」的文件，而此前只能导出外挂
    /// 字幕，用户还得自己再跑一遍 ffmpeg。这里 `-c copy` 不重编码，大文件也是秒级。
    pub(crate) fn export_embedded_subtitles(&mut self, cx: &mut Context<Self>) {
        const MEDIA_EXT: [&str; 9] = [
            "mp4", "mkv", "mov", "avi", "flv", "webm", "m4v", "ts", "wmv",
        ];
        let Some(media) = self.state.selected_file.clone() else {
            self.notice = Some("内嵌字幕需要先打开一个视频文件".to_string());
            cx.notify();
            return;
        };
        let is_media = media
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| MEDIA_EXT.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !is_media || !media.exists() {
            self.notice = Some("当前选中的不是视频文件（导入的字幕没有媒体可封装）".to_string());
            cx.notify();
            return;
        }
        if self.state.segments.is_empty() {
            return;
        }

        let style = self.state.config.subtitle_style.clone();
        let segments = self.state.segments.clone();
        let packs = self.state.translation_packs();
        let source_lang = crate::app::state::source_language_label(&segments)
            .unwrap_or("原文")
            .to_string();
        let stem = media
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("subtitles")
            .to_string();
        let out = media.with_file_name(format!("{stem}.embedded.mkv"));
        // 下划线而不是连字符：启动清扫只认 `v2w_` / `voice2word_` 前缀，
        // 旧名字 `v2w-embed-*` 永远不会被清，崩溃一次就永久残留。
        let dir = std::env::temp_dir().join(format!("v2w_embed_{}", std::process::id()));

        let mut tracks: Vec<(String, std::path::PathBuf)> = Vec::new();
        let mut build = || -> anyhow::Result<()> {
            std::fs::create_dir_all(&dir)?;
            // 第 0 轨：原文
            let mut src_segs = segments.clone();
            for seg in src_segs.iter_mut() {
                seg.show_source_only();
            }
            let src_path = dir.join("00-source.srt");
            crate::subtitle::SubtitleWriter::write_to_file_with_style(
                &src_segs,
                &src_path,
                "srt",
                crate::subtitle::ExportMode::RawOnly,
                &style,
            )?;
            tracks.push((source_lang.clone(), src_path));
            // 其余轨：每种语言各一份
            for (i, (lang, _)) in packs.iter().enumerate() {
                let mut segs = segments.clone();
                for seg in segs.iter_mut() {
                    if !seg.show_translation_pack(lang) {
                        seg.show_source_only();
                    }
                }
                let path = dir.join(format!("{:02}-{i}.srt", i + 1));
                crate::subtitle::SubtitleWriter::write_to_file_with_style(
                    &segs,
                    &path,
                    "srt",
                    crate::subtitle::ExportMode::TranslationOnly,
                    &style,
                )?;
                tracks.push((lang.clone(), path));
            }
            Ok(())
        };
        if let Err(e) = build() {
            self.notice = Some(format!("内嵌字幕导出失败：{e}"));
            cx.notify();
            return;
        }

        let ffmpeg_path =
            crate::utils::config::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        self.notice = Some(format!(
            "正在封装 {} 条字幕轨到 {}…",
            tracks.len(),
            out.display()
        ));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let engine = crate::engines::FFmpegEngine::new(ffmpeg_path);
                    engine.mux_subtitles(&media, &tracks, &out)?;
                    Ok::<std::path::PathBuf, anyhow::Error>(out)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(path) => {
                        info!("内嵌字幕视频已导出: {:?}", path);
                        this.notice = Some(format!("已导出内嵌字幕视频：{}", path.display()));
                    }
                    Err(e) => {
                        this.notice = Some(format!("内嵌字幕导出失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 导出剪映草稿至指定独立文件夹
    pub(crate) fn export_jianying_folder(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }

        let segments = self.state.segments.clone();
        let mode = self.editor_export_mode;
        let video_path = self.state.selected_file.clone();
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Voice2Word")
            .to_string();
        // 媒体时长在 `move` 闭包外取：闭包里拿不到 `self`，而草稿的视频轨必须用
        // 真实媒体时长（不能用最后一句字幕的 end，否则素材会被裁短）。
        let media_duration = self.state.total_duration;

        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择剪映草稿保存目录")
                .pick_folder()
                .await;
            if let Some(folder_handle) = handle {
                let target_dir = folder_handle.path().join(&stem);
                let res = cx
                    .background_executor()
                    .spawn(async move {
                        let media = crate::subtitle::jianying::DraftMedia {
                            path: video_path.clone(),
                            duration_secs: (media_duration > 0.0).then_some(media_duration),
                        };
                        crate::subtitle::JianYingExporter::export_to_folder_with_media(
                            &segments,
                            &media,
                            &target_dir,
                            &stem,
                            mode,
                        )
                    })
                    .await;

                let _ = this.update(cx, |this, cx| {
                    match res {
                        Ok(p) => {
                            info!("剪映草稿文件夹导出成功: {:?}", p);
                            let _ = std::process::Command::new("explorer").arg(&p).spawn();
                            this.notice = Some(format!(
                                "已导出剪映草稿到 {}",
                                p.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| p.display().to_string())
                            ));
                        }
                        Err(e) => {
                            this.state.status =
                                ProcessStatus::Failed(format!("剪映草稿导出失败：{}", e));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 导出 FCPXML (Final Cut Pro / 达芬奇) 工程文件
    pub(crate) fn export_fcpxml(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        let mode = self.editor_export_mode;
        self.export_with_save_dialog(
            cx,
            crate::subtitle::writer::export_file_name(
                &self.state.config.ui.export_name_template,
                &stem,
                "fcpxml",
                &chrono::Local::now().format("%Y%m%d").to_string(),
            ),
            "Final Cut Pro XML (*.fcpxml)",
            "fcpxml".to_string(),
            move |segs, path| {
                crate::subtitle::FcpXmlExporter::write_to_file(segs, path, &stem, mode)
            },
        );
    }

    /// 导出 Adobe Premiere Pro XML (FCP7 XML / xmeml) 工程文件
    pub(crate) fn export_premiere_xml(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        let mode = self.editor_export_mode;
        self.export_with_save_dialog(
            cx,
            crate::subtitle::writer::export_file_name(
                &self.state.config.ui.export_name_template,
                &stem,
                "xml",
                &chrono::Local::now().format("%Y%m%d").to_string(),
            ),
            "Premiere Pro XML (*.xml)",
            "xml".to_string(),
            move |segs, path| {
                crate::subtitle::PremiereXmlExporter::write_to_file(segs, path, &stem, mode)
            },
        );
    }

    /// 播放当前工作区的媒体，**画面与字幕都由本应用自绘**。
    ///
    /// # 为什么不再外挂 ffplay
    ///
    /// 此前这条路是起一个 `ffplay.exe -vf subtitles=...` 进程：它会额外弹一个控制台
    /// 窗口，而且字幕是靠滤镜**重新渲染一遍**——与画面上的样式未必一致，用户看到的
    /// 是「另一个东西在放」。剪辑台的监视器本来就按 `subtitle_style` 自绘字幕
    /// （见 `render_monitor_picture` 与舞台上的字幕框），那份才与导出同源。
    ///
    /// 内嵌播放器画在剪辑台的监视器上，所以这里必须先切到剪辑台：本按钮挂在底部
    /// 状态栏（只在语音转写页存在），不切页的话用户点了播放却看不到任何东西，
    /// 表现就是「点了没反应」——这正是它当初被做成独立 ffplay 窗口的原因。
    pub(crate) fn play_video(&mut self, cx: &mut Context<Self>) {
        if self.state.preview_media().is_none() {
            self.state.status = ProcessStatus::Failed("请先载入视频，再播放预览".to_string());
            cx.notify();
            return;
        }
        if self.state.segments.is_empty() {
            self.state.status = ProcessStatus::Failed("请先完成字幕识别，再播放预览".to_string());
            cx.notify();
            return;
        }
        self.state.active_tab = WorkspaceTab::Editor;
        if !self.state.is_playing {
            self.toggle_play_preview(cx);
        }
        cx.notify();
    }

    /// 请求终止当前转写任务：标记取消并强杀识别子进程。
    ///
    /// 状态先切到「终止中」给用户即时反馈，真正的复位由管线的收尾事件统一完成
    /// （见 `start_processing` 里 `cancel_requested` 的分支），避免两处各写一套收尾。
    /// 未在转写时调用是安全的空操作。
    pub(crate) fn request_cancel_processing(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        self.state.cancel_requested = true;
        self.state.status = ProcessStatus::Processing {
            stage: "终止中".to_string(),
            progress: 1.0,
            detail: "正在终止识别进程，请稍候...".to_string(),
        };
        self.state.pipeline.cancel();
        cx.notify();
    }

    /// 异步探测当前视频的真实分辨率（驱动监视器画面等比适配，杜绝字幕掉进黑边、画面拉伸变形）
    pub(crate) fn probe_video_dimensions(&mut self, cx: &mut Context<Self>) {
        let Some(video) = self.state.preview_media().cloned() else {
            return;
        };
        if self.state.dims_probed_for.as_ref() == Some(&video) {
            return;
        }
        self.state.dims_probed_for = Some(video.clone());
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ffmpeg.get_resolution(&video) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some((w, h)) = result {
                    this.state.video_width = w;
                    this.state.video_height = h;
                    this.state.video_player.set_frame_dimensions(w, h);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 异步提取当前工程的音频波形包络，驱动时间轴波形轨。
    ///
    /// 与 `probe_video_dimensions` 同样是「按媒体路径去重」的惰性任务：
    /// 来源一致直接复用已有包络；来源变了先清空再重算，避免时间轴在新波形
    /// 到达前画出上一支视频的包络（时间刻度完全对不上）。
    pub(crate) fn ensure_waveform(&mut self, cx: &mut Context<Self>) {
        let Some(media) = self.state.preview_media().cloned() else {
            return;
        };
        if !media.is_file() {
            return;
        }
        if self.state.waveform_ready_for(&media) {
            return;
        }
        if self.state.waveform_busy_for.as_ref() == Some(&media) {
            return;
        }

        self.state.waveform = None;
        self.state.waveform_busy_for = Some(media.clone());

        let ffmpeg = crate::engines::FFmpegEngine::new(crate::utils::AppConfig::resolve_path(
            &self.state.config.paths.ffmpeg,
        ));
        let buckets = crate::engines::waveform::MAX_WAVEFORM_BUCKETS;
        let extract_target = media.clone();
        // 波形提取要把**整段**音轨解码成 PCM 再分桶聚合：长视频是几十秒级
        // 的 CPU 活（见 `utils::background_thread` 的 DEDICATED_LONG_TASKS）。
        // 不能走 `background_executor`（Windows 上 GPUI 一律按 High 派发，
        // 会和转写抢 CPU），改走专用线程 + BELOW_NORMAL；结果照旧回 UI 写状态。
        let result_rx =
            crate::utils::background_thread::spawn_long_task("v2w-waveform-extract", move || {
                crate::engines::waveform::extract(&ffmpeg, &extract_target, buckets)
            });
        cx.spawn(async move |this, cx| {
            let result = match result_rx.await {
                Ok(result) => result,
                // 线程起不来 / panic：按「提取失败」处理，时间轴不显示波形轨。
                Err(_) => Err(anyhow::anyhow!("波形提取线程异常退出")),
            };
            let _ = this.update(cx, |this, cx| {
                if this.state.waveform_busy_for.as_ref() == Some(&media) {
                    this.state.waveform_busy_for = None;
                }
                match result {
                    Ok(data) => {
                        info!(buckets = data.peaks.len(), "时间轴波形包络提取完成");
                        this.state.waveform = Some(std::sync::Arc::new(data));
                    }
                    // 波形只是辅助视觉，失败就静默不显示，绝不能因此打断转写主流程
                    Err(error) => {
                        tracing::warn!(error = %error, "波形提取失败，时间轴将不显示波形轨");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 异步根据当前播放时间抽取单帧画面（单飞队列：最多 1 个 FFmpeg 实例并发，合并多余拖动请求）
    pub(crate) fn trigger_extract_frame(&mut self, cx: &mut Context<Self>) {
        self.probe_video_dimensions(cx);
        let Some(video_path) = self.state.preview_media().cloned() else {
            return;
        };
        if !video_path.exists() {
            return;
        }
        let target_time = self.state.current_time;

        // 记录最新期望的时间戳
        {
            let mut pending = self
                .pending_extract_time
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *pending = Some(target_time);
        }

        // 如果当前已有后台抽帧 worker 在运行，直接返回！运行中的 worker 抽完后会自动接取 pending_time
        if self
            .is_extracting_frame
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }

        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffmpeg = std::sync::Arc::new(crate::engines::FFmpegEngine::new(ffmpeg_path));
        let cache = self.state.frame_cache.clone();
        let is_extracting = self.is_extracting_frame.clone();
        let pending_time = self.pending_extract_time.clone();

        cx.spawn(async move |this, cx| {
            loop {
                // 取出当前最新的目标时间
                let next_time = {
                    let mut lock = pending_time.lock().unwrap_or_else(|e| e.into_inner());
                    lock.take()
                };

                let time = match next_time {
                    Some(t) => t,
                    None => {
                        // 无待处理请求，释放锁并退出
                        is_extracting.store(false, std::sync::atomic::Ordering::SeqCst);
                        // 双重检查避免竞态退出
                        let has_more = pending_time
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_some();
                        if has_more
                            && is_extracting
                                .compare_exchange(
                                    false,
                                    true,
                                    std::sync::atomic::Ordering::SeqCst,
                                    std::sync::atomic::Ordering::SeqCst,
                                )
                                .is_ok()
                        {
                            continue;
                        }
                        break;
                    }
                };

                let video_clone = video_path.clone();
                let ffmpeg_clone = ffmpeg.clone();
                let cache_clone = cache.clone();

                // 抽帧是「每次拖动都要等」的短任务（几十到几百毫秒）且单飞串行，
                // 留在后台执行器即可；该走专用低优先级线程的是「整片代理转码 / 整段
                // 波形解码 / 全文件哈希」这类几十秒起步的长活（见
                // `utils::background_thread` 的 `DEDICATED_LONG_TASKS`）。
                let frame_result =
                    cx.background_executor()
                        .spawn(async move {
                            cache_clone.get_or_extract(&video_clone, time, &ffmpeg_clone)
                        })
                        .await;

                let is_latest = {
                    let lock = pending_time.lock().unwrap_or_else(|e| e.into_inner());
                    lock.is_none()
                };

                match frame_result {
                    Ok(frame_path) => {
                        // 抽到帧就说明抽帧链路是通的：清零「连续失败」计数，下一次再连续
                        // 失败会重新给一次提示（计数为什么放在进程级静态量里，见
                        // `FRAME_FAILURE_STREAK`）。
                        FRAME_FAILURE_STREAK.store(0, std::sync::atomic::Ordering::Relaxed);
                        // 仅当当前抽取结果依然是最新位置时才提交 UI 渲染，杜绝旧帧闪现与滞后延迟感
                        if is_latest {
                            // 顺手把这一帧解码成 GPUI 能直接上传的纹理数据。
                            // 解码放后台线程：960 宽的 JPG 解一次十几毫秒，丢给渲染线程
                            // 就是每次换帧稳定掉一帧；解码失败返回 None，渲染侧回退到
                            // 「按路径交给 GPUI 加载」的老行为，功能不受影响。
                            let decode_path = frame_path.clone();
                            let decoded = cx
                                .background_executor()
                                .spawn(async move {
                                    crate::ui::editor::render_image_from_frame_file(&decode_path)
                                })
                                .await;
                            let _ = this.update(cx, |this, cx| {
                                // 在这里判一次存在性：`get_or_extract` 正常返回的路径都已落盘，
                                // 但磁盘帧目录会按「最近使用」裁剪——把存在性收敛到后台更新点，
                                // 渲染路径就不必每帧对预览帧 `path.exists()`。
                                this.state.preview_frame_path = if frame_path.exists() {
                                    Some(frame_path)
                                } else {
                                    None
                                };
                                // 旧帧在这里只是「放下」（Arc 少一个引用），真正的纹理回收
                                // 在渲染侧换图时做——只有那里拿得到 `&mut Window`，
                                // 能调 `drop_image`（见 render_monitor_picture）。
                                this.state.preview_frame_image =
                                    if this.state.preview_frame_path.is_some() {
                                        decoded
                                    } else {
                                        None
                                    };
                                cx.notify();
                            });
                        }
                        // 预取「下一格」：命中缓存立即返回，未命中才真起一次 FFmpeg。
                        // 用它自己的专用线程、不 await——它属于「提前做」的活，放在
                        // 提交本帧之前等它会拖慢本帧（见 utils::background_thread）。
                        // 只在本帧已是最新位置时预取：不是最新说明用户已经拖到别处，
                        // worker 立刻要去处理新请求，预取只会和它抢 CPU。
                        if is_latest {
                            let prefetch_cache = cache.clone();
                            let prefetch_video = video_path.clone();
                            let prefetch_ffmpeg = ffmpeg.clone();
                            let prefetch_time = time;
                            crate::utils::background_thread::spawn_detached_long_task(
                                "v2w-frame-prefetch",
                                move || {
                                    let _ = prefetch_cache.prefetch_next_tick(
                                        &prefetch_video,
                                        prefetch_time,
                                        &prefetch_ffmpeg,
                                    );
                                },
                            );
                        }
                    }
                    Err(err) => {
                        // 抽帧失败此前被 `if let Ok(...)` 整个吞掉：用户侧只看到「拖时间轴
                        // 画面停在上一帧 / 空白」，没有任何解释。先保证日志，再在**连续**
                        // 失败跨过阈值时给一次可见提示——每条失败都挂会刷屏（拖动一次
                        // 时间轴就能触发几十次抽帧），所以只在阈值那一次提示。
                        let streak = FRAME_FAILURE_STREAK
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            .saturating_add(1);
                        tracing::warn!(error = %err, time_sec = time, streak, "画面预览抽帧失败");
                        if let Some(msg) = frame_failure_notice(streak) {
                            let _ = this.update(cx, |this, cx| {
                                // 走中性提示条（notice）：取帧失败不影响转写与导出，
                                // 不该染成 ProcessStatus::Failed 的错误态。
                                this.notice = Some(msg.to_string());
                                cx.notify();
                            });
                        }
                    }
                }
            }
        })
        .detach();
    }

    pub(crate) fn halt_preview_playback(&mut self) {
        if self.state.is_playing {
            self.state.current_time = self.state.video_player.current_play_time();
            self.state.is_playing = false;
        }
        // 暂停/停止/跳转都会经过这里，属于高频交互：登记「稍后落库」，
        // 别在这里序列化整份工程（见 SEGMENTS_FLUSH_IDLE）。
        self.state.defer_segments_flush();
        self.state.video_player.stop();
        self.play_tick_generation = self.state.video_player.generation();
    }

    /// 上一句字幕
    pub(crate) fn jump_prev_segment(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        self.halt_preview_playback();
        let cur_idx = self.state.selected_segment_index.unwrap_or(1);
        let new_idx = if cur_idx > 1 { cur_idx - 1 } else { 1 };
        self.state.select_segment(new_idx);
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 下一句字幕
    pub(crate) fn jump_next_segment(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        self.halt_preview_playback();
        let cur_idx = self.state.selected_segment_index.unwrap_or(1);
        let new_idx = if cur_idx < self.state.segments.len() {
            cur_idx + 1
        } else {
            self.state.segments.len()
        };
        self.state.select_segment(new_idx);
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 切换内嵌实时播放模式 (25fps 动态图像 + FFplay 同步伴音)
    pub(crate) fn toggle_play_preview(&mut self, cx: &mut Context<Self>) {
        if self.state.is_playing {
            // 暂停：先记下墙钟时间，保留最后一帧，避免闪黑或抽帧滞后。
            self.state.current_time = self.state.video_player.current_play_time();
            // 暂停点落在切句边界上时，选中句与画面字幕也要一起落在同一句
            // （判据同 `get_active_segment`）；此刻已停播，编辑缓冲可安全对齐。
            self.state.sync_selection_to_time(self.is_text_focused);
            self.state.is_playing = false;
            self.state.video_player.pause_clock();
            self.trigger_extract_frame(cx);
            cx.notify();
        } else {
            let Some(video_file) = self.state.preview_media().cloned() else {
                return;
            };
            // 开播同样是高频入口（用户会反复暂停/继续对字幕），登记即可。
            self.state.defer_segments_flush();
            self.state.is_playing = true;
            let start_sec = self.state.current_time;
            self.state.video_player.play(video_file, start_sec);
            let tick_gen = self.state.video_player.generation();
            self.play_tick_generation = tick_gen;

            // 起播失败（ffmpeg/ffplay 起不来、文件不可读…）以前**没有任何人读**这个错误：
            // 界面表现就是「点了播放没反应」。这里先同步收一次（覆盖起播即失败的路径），
            // 循环结束时再收一次（覆盖解码线程稍后报错的路径）。
            self.surface_playback_error();

            let player = self.state.video_player.clone();
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(40))
                    .await;
                let should_continue = this
                    .update(cx, |this, cx| {
                        if this.play_tick_generation != tick_gen {
                            return false;
                        }
                        if !this.state.is_playing {
                            return false;
                        }
                        let play_time = player.current_play_time();
                        let max_dur = if this.state.total_duration > 0.0 {
                            this.state.total_duration
                        } else {
                            this.state.segments.last().map(|s| s.end).unwrap_or(0.0)
                        };

                        if play_time >= max_dur && max_dur > 0.0 {
                            this.state.current_time = max_dur;
                            this.state.is_playing = false;
                            this.state.video_player.pause_clock();
                            this.trigger_extract_frame(cx);
                            cx.notify();
                            return false;
                        }

                        this.state.current_time = play_time;
                        // 选中句 / 编辑缓冲跟随画面字幕。判据与监视器覆盖层
                        // 同源（`get_active_segment` 的半开区间），否则在切句边界上，
                        // 右侧列表跳到下一句、画面还在上一句，看着就是对不上。
                        // 用户正在编辑框里打字时不动编辑缓冲，避免冲掉输入。
                        this.state.sync_selection_to_time(this.is_text_focused);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);

                if !should_continue {
                    // 停下来的原因可能是「解码线程报了错并把 is_playing 复位」，
                    // 那种情况必须把原因讲给用户，否则同样是「点了没反应」。
                    let _ = this.update(cx, |this, cx| {
                        if this.surface_playback_error() {
                            cx.notify();
                        }
                    });
                    break;
                }
            })
            .detach();
            cx.notify();
        }
    }

    /// 把播放器的失败原因挂到一次性提示上；返回「这次确实有东西要显示」。
    ///
    /// 判据是「用户能不能自己修」：播放失败几乎总是环境问题（ffmpeg 不可用、
    /// 文件损坏/被占用），错误文本里已经带了具体做法（见 `PlaybackFailure` 的 Display），
    /// 所以原样转达即可；这里不再加工或缩写，免得把可操作的信息磨没。
    ///
    /// 取走即清空（`take_playback_error`）：用户看到一次就够了，反复弹同一句是噪音。
    fn surface_playback_error(&mut self) -> bool {
        let Some(failure) = self.state.video_player.take_playback_error() else {
            return false;
        };
        tracing::warn!(stage = ?failure.stage, path = %failure.path, "预览播放失败");
        self.notice = Some(failure.to_string());
        true
    }

    /// 把当前字幕**烧录**（硬字幕合成）进视频，导出一个自带字幕的新 mp4。
    ///
    /// 这是「导出带字幕的成品视频」入口：与其它字幕导出（写 .srt/.ass 文件）不同，
    /// 它调 FFmpeg 把字幕按当前样式渲染进画面并重新编码。字幕先写成临时 ASS
    /// （内嵌样式，FFmpeg 的 `subtitles` 滤镜据此渲染），烧录完成后立即删除。
    pub(crate) fn export_burned_video(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.state.selected_file.clone() else {
            self.state.status =
                ProcessStatus::Failed("请先打开一个视频文件，再烧录字幕".to_string());
            cx.notify();
            return;
        };
        if self.state.segments.is_empty() {
            return;
        }
        self.state.flush_segments_if_dirty();

        let segments = self.state.segments.clone();
        let mode = self.editor_export_mode;
        let style = self.state.config.subtitle_style.clone();
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let stem = input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("video")
            .to_string();
        let default_name = format!("{stem}_hardsub.mp4");

        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择字幕烧录视频的保存位置")
                .set_file_name(&default_name)
                .add_filter("MP4 视频", &["mp4"])
                .save_file()
                .await;
            let Some(save_handle) = handle else {
                return;
            };
            let output = save_handle.path().to_path_buf();

            let res = cx
                .background_executor()
                .spawn(async move {
                    // 临时 ASS：把当前样式写进字幕头，FFmpeg 的 subtitles 滤镜会按
                    // 内嵌样式渲染，实现「预览即所得」的硬字幕。
                    let sub_path = std::env::temp_dir().join(format!(
                        "voice2word_burn_{}_{}.ass",
                        std::process::id(),
                        burn_temp_seq()
                    ));
                    let _ = std::fs::remove_file(&sub_path);
                    // 临时字幕交给 RAII 守卫：即便 write_ass_with_style 中途 `?`
                    // 提前返回，守卫的 `Drop` 也会删掉半截 `.ass`，不在 %TEMP% 累积。
                    let sub_guard = crate::utils::TempPathGuard::file(&sub_path);
                    SubtitleWriter::write_ass_with_style(&segments, &sub_path, mode, &style)?;
                    let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
                    let result = ffmpeg.burn_subtitles(
                        &input,
                        &sub_path,
                        &output,
                        &crate::engines::ffmpeg::BurnOptions::default(),
                    );
                    // 无论成败都清掉临时字幕，避免在 %TEMP% 里累积
                    drop(sub_guard);
                    result
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(path) => {
                        info!("字幕烧录视频导出成功: {:?}", path);
                        // 成品可能较大，直接打开所在目录方便用户找到文件
                        if let Some(dir) = path.parent() {
                            let _ = std::process::Command::new("explorer").arg(dir).spawn();
                        }
                        this.notice = Some(format!(
                            "已烧录字幕并导出视频：{}",
                            path.file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.display().to_string())
                        ));
                        this.state.status = ProcessStatus::Idle;
                    }
                    Err(e) => {
                        tracing::error!("字幕烧录失败: {:?}", e);
                        this.state.status = ProcessStatus::Failed(format!("字幕烧录失败：{}", e));
                        this.notice = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 「导出片段」：把当前选中句切成一段视频（`.mp4`）。
    pub(crate) fn export_selected_clip(&mut self, cx: &mut Context<Self>) {
        self.export_segment_clip(crate::engines::ffmpeg::ClipKind::Video, cx);
    }

    /// 「导出音频」：把当前选中句切成 16kHz 单声道 WAV（与识别输入同规格，便于复用/复听）。
    pub(crate) fn export_selected_clip_audio(&mut self, cx: &mut Context<Self>) {
        self.export_segment_clip(crate::engines::ffmpeg::ClipKind::Audio, cx);
    }

    /// 「选中句 → 片段」的共享实现：纯函数规划 → 保存对话框 → 后台切片 → 一次性提示。
    ///
    /// 覆盖策略由保存对话框负责：原生对话框在目标文件已存在时会先弹「是否覆盖」，
    /// 用户确认后才返回路径，因此 FFmpeg 那边只是带上 `-y` 免得无人应答的 stdin
    /// 把它挂住，不是静默覆盖。
    fn export_segment_clip(
        &mut self,
        kind: crate::engines::ffmpeg::ClipKind,
        cx: &mut Context<Self>,
    ) {
        use std::sync::atomic::Ordering;
        if CLIP_EXPORT_BUSY.load(Ordering::Relaxed) {
            return;
        }
        let Some(input) = self.state.selected_file.clone() else {
            self.notice = Some("请先打开一个媒体文件，再导出片段".to_string());
            cx.notify();
            return;
        };
        // 先把编辑缓冲落进 segments，再取选中句，避免用到过期的时间轴。
        self.state.flush_segments_if_dirty();
        let Some(segment) = self
            .state
            .selected_segment_index
            .and_then(|idx| self.state.segments.iter().find(|s| s.index == idx))
            .cloned()
        else {
            self.notice = Some("请先选中一句字幕，再导出片段".to_string());
            cx.notify();
            return;
        };

        // 规划失败（时间戳非法）在这里就定性，不弹保存对话框。
        let plan = match crate::engines::ffmpeg::plan_segment_clip(
            &segment,
            self.state.total_duration,
            crate::engines::ffmpeg::DEFAULT_CLIP_PAD,
            kind,
        ) {
            Ok(plan) => plan,
            Err(e) => {
                self.notice = Some(format!("片段导出失败：{e}"));
                cx.notify();
                return;
            }
        };
        // 该句时间轴无效（end <= start）时规划会兜底到 MIN_EDIT_DUR，提示里说明，
        // 免得用户以为导出的是一段正常时长的话。
        let degenerate = segment.end <= segment.start;

        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let stem = input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("clip")
            .to_string();
        let ext = kind.extension();
        let default_name = format!("{stem}_line{:03}.{ext}", segment.index);
        let start_sec = plan.start_sec;
        let end_sec = plan.start_sec + plan.duration_sec;
        let audio = matches!(kind, crate::engines::ffmpeg::ClipKind::Audio);

        CLIP_EXPORT_BUSY.store(true, Ordering::Relaxed);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择片段保存位置")
                .set_file_name(&default_name)
                .add_filter(if audio { "WAV 音频" } else { "MP4 视频" }, &[ext])
                .save_file()
                .await;
            let Some(save_handle) = handle else {
                // 用户取消：复位忙标志，不打扰。
                let _ = this.update(cx, |_this, cx| {
                    CLIP_EXPORT_BUSY.store(false, Ordering::Relaxed);
                    cx.notify();
                });
                return;
            };
            let output = save_handle.path().to_path_buf();

            let res = cx
                .background_executor()
                .spawn(async move {
                    let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
                    ffmpeg.export_clip(&input, &plan, &output)
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                CLIP_EXPORT_BUSY.store(false, Ordering::Relaxed);
                match res {
                    Ok(path) => {
                        info!("片段导出成功: {:?}", path);
                        let note = if degenerate {
                            "（原句时间轴无效，已按最短片段兜底）"
                        } else {
                            ""
                        };
                        this.notice = Some(format!(
                            "已导出片段{note}：{}（{:.2}s → {:.2}s）",
                            path.display(),
                            start_sec,
                            end_sec
                        ));
                    }
                    Err(e) => {
                        tracing::error!("片段导出失败: {:?}", e);
                        // 失败不静默：错误里已带 FFmpeg 的 stderr 尾部，直接进提示。
                        this.notice = Some(format!("片段导出失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 根据当前剪辑工作台选中的格式执行统一导出
    pub(crate) fn perform_editor_export(&mut self, cx: &mut Context<Self>) {
        self.state.flush_segments_if_dirty();
        if self.state.segments.is_empty() {
            return;
        }

        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        match self.editor_export_format {
            EditorExportFormat::JianYing => self.export_jianying_local(cx),
            EditorExportFormat::JianYingFolder => self.export_jianying_folder(cx),
            EditorExportFormat::Fcpxml => self.export_fcpxml(cx),
            EditorExportFormat::PremiereXml => self.export_premiere_xml(cx),
            EditorExportFormat::BurnVideo => self.export_burned_video(cx),
            EditorExportFormat::Srt
            | EditorExportFormat::Ass
            | EditorExportFormat::Txt
            | EditorExportFormat::Vtt
            | EditorExportFormat::Json
            | EditorExportFormat::EbuTtD
            | EditorExportFormat::NetflixTtal
            | EditorExportFormat::Sbv
            | EditorExportFormat::Lrc
            | EditorExportFormat::TxtTs => {
                // 枚举只提供**格式名**，扩展名与对话框过滤器名一律交给
                // `export_spec_for` 求——这两份字面量只存在于 `writer.rs` 一处，
                // 不会再与 writer 的分派表漂移。
                let fmt = editor_export_format_string(self.editor_export_format);
                let (ext, filter_label) = export_spec_for(fmt);
                // 直接按目标扩展名导出，不再临时改写全局 output_format（避免副作用泄漏到后续管线调用）
                // ASS 会带上主界面配置的字幕样式（字号/字间距/底边距/预设配色）
                let style = self.state.config.subtitle_style.clone();
                let mode = self.editor_export_mode;
                self.export_with_save_dialog(
                    cx,
                    crate::subtitle::writer::export_file_name(
                        &self.state.config.ui.export_name_template,
                        &stem,
                        ext,
                        &chrono::Local::now().format("%Y%m%d").to_string(),
                    ),
                    filter_label,
                    ext.to_string(),
                    move |segs, path| {
                        // 写盘用 **格式名**（`fmt`）而不是扩展名（`ext`）：`txt` 与
                        // `txt-ts` 同扩展名 `.txt`，只有按格式名写出才会带每行时间码。
                        // 其余格式二者取值相同，写出字节不变。
                        SubtitleWriter::write_to_file_with_style(segs, path, fmt, mode, &style)
                    },
                );
            }
        }
    }

    /// 触发大模型多语言字幕后台流式翻译。
    ///
    /// 引擎按 `state.translate_mode` 二选一：本地 Qwen（llama.cpp）或在线
    /// OpenAI 兼容接口；两条链路的进度回调协议一致，UI 侧无需分支。
    pub(crate) fn trigger_llm_translation(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() || self.state.is_translating {
            return;
        }
        // 选句模式：只译勾选的那几句。一句都没勾时**不发请求**，直接把话说清楚——
        // 静默返回会让用户以为按钮坏了。
        let picked: Option<Vec<i64>> = self
            .state
            .translate_pick
            .as_ref()
            .map(|set| set.iter().copied().collect());
        if let Some(ids) = picked.as_ref() {
            if ids.is_empty() {
                self.state.translate_status_msg =
                    "选句翻译：请先在下方字幕清单里勾选要翻译的句子".to_string();
                cx.notify();
                return;
            }
        }
        // 目标语言 == 视频语言：这不是翻译而是复制。引擎会把每一句都判成「复制原文」，
        // 最后整片报「未能译出任何一句」——在入口就说清楚，别让用户白等一轮。
        if self.state.translate_target_equals_source() {
            self.state.translate_status_msg = format!(
                "目标语言与视频语言相同（{}）：请先改用下拉框选择要译成的语言",
                self.state.translate_target_lang
            );
            cx.notify();
            return;
        }
        // 本轮参与翻译的句子：选句模式只取勾选的，否则整篇。
        // 引擎按 `index` 回填、收尾按 `start` 合并，因此传子集是安全的。
        let work_segments: Vec<crate::subtitle::Segment> = match picked.as_ref() {
            Some(ids) => self
                .state
                .segments
                .iter()
                .filter(|s| ids.contains(&(s.index as i64)))
                .cloned()
                .collect(),
            None => self.state.segments.clone(),
        };
        // 「已完成」必须按**当前目标语言**判定。用不分语言的 `translated_count()`
        // 会踩到这个坑：用户把目标语言从 English 改成 日本語 后，旧英文译文仍算
        // 「已完成」，于是 100% 时这里直接 return——点「开始翻译」毫无反应，
        // 用户完全不知道为什么。现在只有「当前语言全部译完」才提前返回。
        // 另：这里与收尾文案用**同一套**统计口径，避免“按钮说已完成、收尾又说部分完成”自相矛盾。
        let target_now = self.state.translate_target_lang.clone();
        // 「已完成」用**实际译出**口径：分子只认非空译文 + 目标语言匹配，
        // 分母只算源文非空的句子（空源句没有可译内容，计入分母会永远差几句）。
        let done_now = translated_out_count(&work_segments, &target_now);
        let expected_now = expected_translation_count(&work_segments);
        if expected_now == 0 {
            self.state.translate_status_msg = "没有可翻译的文本（所有字幕行都是空的）".to_string();
            cx.notify();
            return;
        }
        if done_now >= expected_now {
            self.state.translate_status_msg =
                format!("{} 句已全部翻译为{}", expected_now, target_now);
            cx.notify();
            return;
        }

        let mode = self.state.translate_mode;
        self.state.is_translating = true;
        self.state.translate_progress = 0.0;
        self.state.translate_status_msg = match mode {
            crate::engines::TranslateMode::OnlineApi => {
                format!(
                    "正在初始化在线翻译引擎（{}）...",
                    self.state.config.translate.api_model
                )
            }
            crate::engines::TranslateMode::OfflineQwen => "正在初始化 Qwen 翻译引擎...".to_string(),
        };
        cx.notify();

        // 收尾合并的冲突基线（S4）+ 文档代次（S1）：翻译要跑几分钟，这段时间里
        // 用户可能改了原文/手写译文，也可能直接切到别的工程。没有这两样，收尾
        // 就会把机翻结果盖到用户的手工劳动上，或写进另一个文档的库行。
        let baseline = std::sync::Arc::new(crate::app::state::TranslationBaseline::capture(
            &work_segments,
        ));
        let epoch_at_start = self.state.doc_epoch;
        let segments = work_segments;
        let target_lang = self.state.translate_target_lang.clone();
        // 另存一份给收尾统计用：`target_lang` 会被 move 进下面的异步块。
        // 收尾必须按本轮目标语言数：否则用户换语言后，旧语言的译文
        // 会被当成本轮成果，界面又会说「已译出 N 句」而实际一句没译。
        let target_lang_for_msg = target_lang.clone();
        let online_cfg = self.state.online_translate_config();
        let llama_cli =
            crate::utils::config::AppConfig::resolve_path(&self.state.config.paths.llama_cli);
        let llm_model =
            crate::utils::config::AppConfig::resolve_path(&self.state.config.paths.llm_model);
        let llm_ctx = self.state.config.pipeline.llm_ctx;
        let llm_threads = self.state.config.pipeline.llm_threads;
        // 术语表提示：离线链路要显式注入到 LLM 引擎（在线链路已随 online_cfg 一起带）。
        let glossary_hint = self.state.translate_glossary_hint();
        // 取消标志：任务起手先复位（上一轮取消后它会停在 true），再交给引擎
        let cancel_flag = self.translate_cancel.clone();
        cancel_flag.store(false, std::sync::atomic::Ordering::SeqCst);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(f64, String)>();
        // 部分结果回传槽：在线链路整片中终止前会把已译好的部分放进这里。任务期间
        // `segments` 已被移进引擎，失败分支若不预留这个槽，本次已算好、已计费的
        // 译文就会随一次网络抖动全部作废。
        let partial_sink = crate::engines::translate::PartialSink::default();
        let partial_sink_for_task = partial_sink.clone();

        // 增量落盘（S3）：翻译要跑几分钟，此前译文只在收尾写库，中途关窗/崩溃等于
        // 整段工作归零。这里每 3 秒把引擎刷新的增量结果合并进当前字幕表并落库；
        // 文档已切换或任务已结束就立刻停（S1：绝不把结果写进别的文档）。
        {
            let sink = partial_sink.clone();
            let base = baseline.clone();
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                let snapshot = {
                    let guard = sink.lock().unwrap_or_else(|e| e.into_inner());
                    guard.clone()
                };
                let keep_running = this
                    .update(cx, |this, cx| {
                        if this.state.doc_epoch != epoch_at_start || !this.state.is_translating {
                            return false;
                        }
                        if let Some(done) = snapshot {
                            this.state
                                .merge_translations_checked(done, Some(base.as_ref()));
                            this.state.segments_dirty = true;
                            this.state.flush_segments_if_dirty();
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            })
            .detach();
        }

        cx.spawn(async move |this, cx| {
            let handle = cx.background_executor().spawn(async move {
                let mut translate_engine = match mode {
                    crate::engines::TranslateMode::OnlineApi => {
                        crate::engines::TranslateEngine::online(online_cfg)
                    }
                    crate::engines::TranslateMode::OfflineQwen => {
                        let llm_engine = crate::engines::LLMEngine::new(
                            llama_cli,
                            llm_model,
                            llm_ctx,
                            llm_threads,
                        );
                        llm_engine.set_glossary(&glossary_hint);
                        crate::engines::TranslateEngine::offline(llm_engine)
                    }
                };
                translate_engine.set_partial_sink(partial_sink_for_task);
                let tx_clone = tx.clone();
                translate_engine.translate_subtitles(
                    segments,
                    &target_lang,
                    Some(Box::new(move |p, msg| {
                        let _ = tx_clone.send((p, msg.to_string()));
                    })),
                    cancel_flag,
                )
            });

            let progress_this = this.clone();
            cx.spawn(async move |cx| {
                while let Some((progress, msg)) = rx.recv().await {
                    let _ = progress_this.update(cx, |this, cx| {
                        // 收尾已经写过最终文案：通道里残留的进度消息不得把它覆盖回去
                        // （否则「翻译完成（N 句）」会被一条滞后的「在线翻译中: 120/200」
                        // 冲掉，用户以为没翻完）。
                        if !this.state.is_translating {
                            return;
                        }
                        this.state.translate_progress = progress as f32;
                        this.state.translate_status_msg = msg;
                        cx.notify();
                    });
                }
            })
            .detach();

            let result = handle.await;
            // 取消位要在 update 闭包里读、在闭包里复位——中间不能被别的任务改掉
            let was_cancelled = this
                .update(cx, |this, cx| {
                    let c = this
                        .translate_cancel
                        .load(std::sync::atomic::Ordering::SeqCst);
                    this.state.is_translating = false;
                    this.translate_cancel
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    cx.notify();
                    c
                })
                .unwrap_or(false);
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(translated_segs) => {
                        // 收尾统计用「实际译出」口径，而不是「引擎有没有给语言标签」：
                        // 引擎解析失败时可能写下空串/纯空白译文（照样带 translation_lang），
                        // 只按标签计数就会把一片空白显示成「翻译完成」。
                        // `translation_coverage` 的分子只认 `has_translation()`（忽略空串/纯空白），
                        // 分母只算源文非空的句子。
                        let (done, expected) =
                            translation_coverage(&translated_segs, &target_lang_for_msg);
                        // 引擎在取消时返回**已完成的偏序结果**而非错误，所以这里必须
                        // 显式区分「跑完了」与「被取消了」。不区分的话，用户点了取消
                        // 却看到「翻译完成（N 句）」，会以为取消没生效。
                        if was_cancelled {
                            info!("字幕多语言翻译已取消（已完成 {} 句）", done);
                            this.state.translate_status_msg =
                                format!("已取消翻译（保留已完成的 {} 句）", done);
                        } else if expected == 0 || done >= expected {
                            // 全部译出（或整片都是空源句， expected == 0，本轮无遗留）
                            info!("字幕多语言翻译完成（{}/{} 句带译文）", done, expected);
                            this.state.translate_progress = 1.0;
                            this.state.translate_status_msg = format!("翻译完成（{} 句）", done);
                        } else if done == 0 {
                            // 一句都没译出：引擎本该返 Err（见 engines 的 fail-fast），
                            // 这里再兜一层——只要没有任何实际译文就绝不能显示「完成」，
                            // 否则就是「点了翻译、界面说成功、字幕全是空白」的假成功。
                            let msg = format!(
                                "翻译失败：应有 {expected} 句译文，但引擎未译出任何一句。请检查翻译引擎配置（在线模式看地址/密钥/模型名，离线模式看 Qwen 模型与 llama.cpp）后重试。"
                            );
                            tracing::warn!("{}", msg);
                            // 统一收口（见 AppState::note_translate_failure）：转写进行中
                            // 时只写翻译面板，绝不覆盖转写进度态。
                            this.state.note_translate_failure(msg);
                        } else {
                            // 部分译出：说清「差多少」，并明确可以再点一次补齐——
                            // 引擎是增量翻译，不会把已译好的重译一遍。
                            let missing = expected - done;
                            info!(
                                "字幕多语言翻译部分完成（{}/{} 句，{} 句未译出）",
                                done, expected, missing
                            );
                            this.state.translate_progress = done as f32 / expected as f32;
                            this.state.translate_status_msg = format!(
                                "翻译部分完成：{done} / {expected} 句已译出，剩余 {missing} 句未译出（可再次点击「开始翻译」补齐）"
                            );
                        }
                        // 译文**按句合并**回当前字幕表，而不是整表覆盖。
                        //
                        // 翻译要跑几分钟，而这段时间里用户仍能在剪辑台编辑字幕
                        // （改错别字、调时间、拆合句）。引擎拿的是开始时的快照，
                        // 若在这里 `= translated_segs` 直接覆盖，用户这几分钟的
                        // 编辑会被静默回滚——改了半天，翻译一结束全没了。
                        // `merge_translations` 只把译文按 index 并回去，原文/时间
                        // 等用户改动原样保留。
                        // S1：收尾前先确认还停在同一个文档上。期间用户导入字幕、
                        // 又跑了一单转写或打开别的工程，这份结果属于上一个文档，
                        // 合并进来就是「译文写进别的工程」的静默数据污染。
                        if this.state.doc_epoch != epoch_at_start {
                            tracing::warn!(
                                "翻译完成时文档已切换，结果已丢弃（不合并、不落库）"
                            );
                            this.state.translate_status_msg =
                                "翻译结果已丢弃：期间切换了工程或重新导入了字幕".to_string();
                            return;
                        }
                        // S4：带冲突判定的合并——期间被改过原文的句子、以及用户手写
                        // 的译文都不覆盖，只并「原文没变且译文没动过」的那些。
                        let skipped = this
                            .state
                            .merge_translations_checked(translated_segs, Some(baseline.as_ref()));
                        if skipped > 0 {
                            tracing::info!("翻译收尾跳过 {skipped} 句：期间原文或译文被编辑过");
                        }
                        // 译出新译文后，若当前处于「仅原文」模式，自动切为「双语对照」，
                        // 让用户一眼在监视器画面与导出栏看到刚刚译出的目标语言。
                        if done > 0 && this.editor_export_mode == crate::subtitle::ExportMode::RawOnly {
                            this.editor_export_mode = crate::subtitle::ExportMode::Bilingual;
                        }
                        // 译文必须落库，否则重启后历史库里的双语对照会凭空消失
                        this.state.segments_dirty = true;
                        this.state.flush_segments_if_dirty();
                    }
                    Err(e) => {
                        // 引擎在「一句都没译出」（密钥/地址/模型名错、Qwen 起不来）时
                        // 会返回带真实原因的 Err（见 engines 的 fail-fast），这里把它
                        // 失败统一走 `note_translate_failure`：它在没有转写在跑时才占用
                        // 全局 `ProcessStatus::Failed` 横幅，避免翻译失败把转写进度态冲掉。
                        //
                        // 但「整片中终止」不等于「本次一无所获」：在线链路按 token 计费，
                        // 中止前往往已经译好若干批。先把那份**部分结果**合并回字幕表
                        // （有就合并、没有就保持原样），再照旧报错——一次网络抖动不该让
                        // 本次已计费的部分译文全部作废。
                        let partial = {
                            let mut slot = partial_sink
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            slot.take()
                        };
                        if let Some(segs) = partial {
                            if segs.iter().any(|s| s.has_translation()) {
                                if this.state.doc_epoch != epoch_at_start {
                                    // 期间切走了工程：这份部分结果属于上一个文档，丢弃。
                                    tracing::warn!("翻译中止但文档已切换，部分译文不予合并");
                                } else {
                                    info!("翻译中止，合并本次已译出的部分译文");
                                    let skipped = this.state.merge_translations_checked(
                                        segs,
                                        Some(baseline.as_ref()),
                                    );
                                    if skipped > 0 {
                                        tracing::info!(
                                            "部分译文合并跳过 {skipped} 句：期间原文或译文被编辑过"
                                        );
                                    }
                                    this.state.segments_dirty = true;
                                    this.state.flush_segments_if_dirty();
                                }
                            }
                        }
                        tracing::warn!("字幕多语言翻译过程报错：{}", e);
                        let msg = format!("翻译失败：{e}");
                        // 统一收口（见 AppState::note_translate_failure）：转写进行中
                        // 时只写翻译面板，绝不覆盖转写进度态。
                        this.state.note_translate_failure(msg);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 取消正在进行的字幕翻译。
    ///
    /// 只置位取消标志：引擎在**每个批次之间**检查（离线 Qwen 与在线 API 两条链路
    /// 都接了），因此最多损失当前一批，已翻译的句子原样保留。任务收尾时统一复位
    /// （见 `trigger_llm_translation` 的 `handle.await` 之后）。
    pub(crate) fn cancel_llm_translation(&mut self, cx: &mut Context<Self>) {
        if !self.state.is_translating {
            return;
        }
        self.translate_cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.state.translate_status_msg = "正在取消翻译，等待当前批次结束…".to_string();
        cx.notify();
    }

    /// 清空当前字幕列表中的全部译文，恢复单语状态。
    ///
    /// 清空整批译文同样是一次可撤销的编辑：快照、清空、落库、审计都在
    /// [`clear_translations_with_undo`] 里。修前这里直接改 `segments`，用户误点
    /// 一下整份译文就没了，连 Ctrl+Z 都救不回来。
    pub(crate) fn clear_all_translations(&mut self, cx: &mut Context<Self>) {
        if self.state.is_translating {
            self.notice = Some("翻译正在进行中，请先取消翻译再清空译文".to_string());
            cx.notify();
            return;
        }
        self.notice = Some(
            if clear_translations_with_undo(&mut self.state) {
                "已清空全部译文，已恢复为单语显示"
            } else {
                "当前没有译文可清空"
            }
            .to_string(),
        );
        cx.notify();
    }

    /// 只清理「译文与原文一字不差」的句子，其余译文一概不动。
    ///
    /// # 为什么需要它
    ///
    /// 小模型（尤其离线 Qwen）摆烂时会把原文原样吐回来：那句译文在屏幕上看与原文
    /// 一模一样，既没法读又让「已全部翻译」的统计失真。此前用户只剩两条路：忍着，
    /// 或者点「清空译文」把**全部**译文（含已逐句校对好的）一起扔掉——后者代价太大。
    ///
    /// # 边界与口径
    ///
    /// - 判据复用管线那份（见 [`is_copied_translation`]）：短句 / 纯拉丁术语放行；
    /// - 走撤销栈：先 `snapshot_for_undo`，`Ctrl+Z` 能把译文原样换回来；
    /// - **无命中时不压撤销点**：否则按一次 `Ctrl+Z` 只会退掉一次什么都没做的操作；
    /// - 只动 `translation` / `translation_lang`，**不碰 `words`**：词级时间戳属于
    ///   原文字幕（卡拉 OK 高亮按它切分），清译文与它无关；既有的「清空全部译文」
    ///   路径同样不动它，两处口径保持一致；
    /// - 审计归到「改译文」（`EditKind::Translation`），与整批清空同一口径。
    pub(crate) fn clear_copied_translations(&mut self, cx: &mut Context<Self>) {
        if self.state.is_translating {
            self.notice = Some("翻译正在进行中，请先取消翻译再清理复制译文".to_string());
            cx.notify();
            return;
        }
        let positions = copied_translation_positions(&self.state.segments);
        if positions.is_empty() {
            self.notice = Some("没有发现「译文与原文一字不差」的句子".to_string());
            cx.notify();
            return;
        }
        self.state.snapshot_for_undo();
        let mut cleared = 0usize;
        for &pos in &positions {
            let Some(seg) = self.state.segments.get_mut(pos) else {
                continue;
            };
            seg.translation = None;
            seg.translation_lang = None;
            cleared += 1;
        }
        // 双语行高 / 译出句数 / 质检「未翻译」都按修订号失效；随后置脏才算「未保存」，
        // 只递增修订号不置脏的话这次清理不会被写回历史库。
        self.state.bump_segments_revision();
        self.state.segments_dirty = true;
        self.state.flush_segments_if_dirty();
        self.state.edit_log.push(EditRecord {
            index: 0,
            kind: EditKind::Translation,
            before: format!("{cleared} 句复制译文"),
            after: String::new(),
            at: chrono::Local::now(),
        });
        self.notice = Some(format!(
            "已清理 {cleared} 句「译文与原文一字不差」的译文，可按 Ctrl+Z 还原"
        ));
        cx.notify();
    }

    /// 循环切换指定片段的说话人 (0 -> 1 -> 2 -> 3 -> 0)
    pub fn cycle_segment_speaker(&mut self, seg_idx: usize, cx: &mut Context<Self>) {
        let Some(pos) = self.state.segments.iter().position(|s| s.index == seg_idx) else {
            return;
        };
        self.state.snapshot_for_undo();
        let next_spk = match self.state.segments[pos].speaker {
            Some(s) => (s + 1) % 4,
            None => 0,
        };
        self.state.segments[pos].speaker = Some(next_spk);
        self.state.bump_segments_revision();
        self.state.segments_dirty = true;
        self.state.flush_segments_if_dirty();
        cx.notify();
    }

    /// 批量重命名说话人角色（如将 "说话人 1" 映射为 "嘉宾张老师"）
    pub fn rename_speaker(&mut self, speaker_id: u32, new_name: &str, cx: &mut Context<Self>) {
        let trimmed = new_name.trim();
        let target_name = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        self.state.snapshot_for_undo();
        let mut count = 0;
        for seg in &mut self.state.segments {
            if seg.speaker == Some(speaker_id) {
                seg.speaker_name = target_name.clone();
                count += 1;
            }
        }
        self.state.bump_segments_revision();
        self.state.segments_dirty = true;
        self.state.flush_segments_if_dirty();
        self.notice = Some(format!(
            "已为说话人 {} 批量更新角色名称（涉及 {count} 个片段）",
            speaker_id + 1
        ));
        cx.notify();
    }

    /// 当前选中的规则集（没选 / 下标越界都按「没选」处理）。
    fn selected_replace_rule_set(&self) -> Option<ReplaceRuleSet> {
        let idx = self.replace_rule_selected?;
        self.replace_rule_sets.get(idx).cloned()
    }

    /// 当前选中句在 `segments` 里的**位置**下标（`None` = 没选中 / 已不存在）。
    fn selected_segment_position(&self) -> Option<usize> {
        let idx = self.state.selected_segment_index?;
        self.state.segments.iter().position(|s| s.index == idx)
    }

    /// 把当前选中的规则集应用到字幕：`scope` 决定全部句还是选中句。
    ///
    /// # 几个刻意的取舍
    ///
    /// - **先在副本上试跑**：`replace_all` 是写操作，而「一处都没命中还压一个撤销点」
    ///   会让 `Ctrl+Z` 变成退掉一次空操作。这里先在克隆出的片段表上跑一遍，真有命中
    ///   才压快照并把结果换进去（多一次克隆，换撤销语义干净）。
    /// - 匹配选项沿用替换面板上的两个勾选（大小写 / 含译文）：用户在同一处就能看清
    ///   「这次会怎么匹配」，不另造一套开关。
    /// - 有命中后置脏 + 递增修订号 + 落库 + 审计，与面板里的「全部替换」逐项对齐。
    pub(crate) fn apply_selected_replace_rule_set(
        &mut self,
        scope: ReplaceRuleScope,
        cx: &mut Context<Self>,
    ) {
        let Some(set) = self.selected_replace_rule_set() else {
            self.replace_rules_status = Some((
                false,
                "先在「规则集」里选一个（点开列表点一下）".to_string(),
            ));
            cx.notify();
            return;
        };
        let options = crate::subtitle::edit::ReplaceOptions {
            case_sensitive: self.replace_case_sensitive,
            include_translation: self.replace_include_translation,
            ..Default::default()
        };
        // 只有「选中句」需要目标下标表；「全部句」走整表入口，不必先铺出一个几万
        // 元素的下标表（点了没用，纯属白分配）。
        let targets: Vec<usize> = match scope {
            ReplaceRuleScope::AllSegments => Vec::new(),
            ReplaceRuleScope::SelectedSentence => match self.selected_segment_position() {
                Some(pos) => vec![pos],
                None => {
                    self.replace_rules_status =
                        Some((false, "先在字幕清单里选中一句，再应用规则".to_string()));
                    cx.notify();
                    return;
                }
            },
        };
        // 试跑：无命中就什么都不做——不压撤销点、不置脏、不落库。
        // 「全部句」走整表入口、「选中句」走按下标的入口：两条路都在
        // `subtitle::edit::replace_all` 上收敛，逐句语义不会有第二份实现。
        let mut trial = self.state.segments.clone();
        let report = match scope {
            ReplaceRuleScope::AllSegments => apply_replace_rule_set(&mut trial, &set, &options),
            ReplaceRuleScope::SelectedSentence => {
                apply_replace_rule_set_to_indices(&mut trial, &targets, &set, &options)
            }
        };
        if report.is_empty() {
            self.replace_rules_status =
                Some((false, format!("规则集「{}」没有找到匹配的内容", set.name)));
            cx.notify();
            return;
        }
        // 此刻 `state.segments` 还是原样：快照拿到的正是「应用之前」的那一份。
        self.state.snapshot_for_undo();
        // 审计里的 `index` 是**句序号**：整批应用记 0（不是针对单句），单句记那句的序号。
        let audit_index = match scope {
            ReplaceRuleScope::AllSegments => 0,
            ReplaceRuleScope::SelectedSentence => targets
                .first()
                .and_then(|&pos| trial.get(pos))
                .map_or(0, |s| s.index),
        };
        self.state.segments = trial;
        self.state.bump_segments_revision();
        self.state.segments_dirty = true;
        self.state.flush_segments_if_dirty();
        self.state.edit_log.push(EditRecord {
            index: audit_index,
            kind: EditKind::Replace,
            before: format!("规则集「{}」", set.name),
            after: format!("{} 处", report.total_hits()),
            at: chrono::Local::now(),
        });
        self.replace_rules_status = Some((true, replace_rule_report_message(&set.name, &report)));
        cx.notify();
    }

    /// 把内存里的规则集写回磁盘；**只把失败挂到状态行**（成功文案由调用方给：
    /// 「刚保存」「刚导入」「刚删除」要说的话不一样）。失败原样返回给调用方。
    pub(crate) fn persist_replace_rule_sets(&mut self) -> Result<(), String> {
        match save_replace_rule_sets_default(&self.replace_rule_sets) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.replace_rules_status = Some((false, err.clone()));
                Err(err)
            }
        }
    }

    /// 「保存」按钮：把当前规则集写回磁盘（`replace_rules.json`）。
    pub(crate) fn save_replace_rule_sets_now(&mut self, cx: &mut Context<Self>) {
        if self.persist_replace_rule_sets().is_ok() {
            self.replace_rules_status = Some((
                true,
                format!(
                    "已保存 {} 个规则集到 {}",
                    self.replace_rule_sets.len(),
                    REPLACE_RULES_FILE
                ),
            ));
        }
        cx.notify();
    }

    /// 把面板里当前的「查找 → 替换」存成一个新规则集（同名覆盖），并写回磁盘。
    ///
    /// 没有这个入口，规则集就只能靠手写 JSON 或从别人那里导入——用户自己在面板里
    /// 试好的一条替换，理应能一键留下来。名字取查找串（见 [`rule_set_name_for`]）。
    pub(crate) fn save_current_replace_as_rule_set(&mut self, cx: &mut Context<Self>) {
        if self.replace_find.is_empty() {
            self.replace_rules_status = Some((false, "先在「查找」里写上要匹配的文字".to_string()));
            cx.notify();
            return;
        }
        let name = rule_set_name_for(&self.replace_find);
        let set = ReplaceRuleSet {
            name: name.clone(),
            rules: vec![ReplaceRule {
                find: self.replace_find.clone(),
                replace: self.replace_with.clone(),
            }],
        };
        match upsert_replace_rule_set(&mut self.replace_rule_sets, set) {
            Ok(pos) => {
                self.replace_rule_selected = Some(pos);
                if self.persist_replace_rule_sets().is_ok() {
                    self.replace_rules_status = Some((
                        true,
                        format!("已保存规则集「{name}」（1 条规则），已写入 {REPLACE_RULES_FILE}"),
                    ));
                }
            }
            Err(err) => self.replace_rules_status = Some((false, err)),
        }
        cx.notify();
    }

    /// 把面板里当前的「查找 → 替换」追加到选中的规则集末尾，并写回磁盘。
    ///
    /// 同一条查找串只留一条规则：重复追加同一条规则会变成「同一处替换两次」，
    /// 用户看到下拉里挂着一串一模一样的条目，纯属噪声。因此同 `find` 视作**改**
    /// （覆盖替换文本），不是**增**。
    pub(crate) fn append_current_replace_rule(&mut self, cx: &mut Context<Self>) {
        if self.replace_find.is_empty() {
            self.replace_rules_status = Some((false, "先在「查找」里写上要匹配的文字".to_string()));
            cx.notify();
            return;
        }
        let Some(pos) = self.replace_rule_selected else {
            self.replace_rules_status = Some((false, "先选一个规则集再追加".to_string()));
            cx.notify();
            return;
        };
        let Some(set) = self.replace_rule_sets.get_mut(pos) else {
            self.replace_rules_status = Some((false, "选中的规则集已不存在".to_string()));
            cx.notify();
            return;
        };
        let name = set.name.clone();
        let find = self.replace_find.clone();
        let replace = self.replace_with.clone();
        match set.rules.iter_mut().find(|rule| rule.find == find) {
            Some(existing) => existing.replace = replace,
            None => set.rules.push(ReplaceRule { find, replace }),
        }
        let count = set.rules.len();
        if self.persist_replace_rule_sets().is_ok() {
            self.replace_rules_status = Some((
                true,
                format!("已把当前替换加入规则集「{name}」（现共 {count} 条）"),
            ));
        }
        cx.notify();
    }

    /// 「删除当前集」：把选中的规则集从列表里删掉并写回磁盘。
    ///
    /// 删除是**即时生效**的（没有二次确认弹窗：这个面板里改的是用户自己攒的小
    /// JSON，删错了重新导入/重建即可），因此文案里必须写清「已删除」而不是沉默。
    pub(crate) fn delete_selected_replace_rule_set(&mut self, cx: &mut Context<Self>) {
        let Some(pos) = self.replace_rule_selected else {
            self.replace_rules_status = Some((false, "先选一个规则集再删除".to_string()));
            cx.notify();
            return;
        };
        let Some(name) = self.replace_rule_sets.get(pos).map(|s| s.name.clone()) else {
            self.replace_rules_status = Some((false, "选中的规则集已不存在".to_string()));
            cx.notify();
            return;
        };
        if !remove_replace_rule_set(&mut self.replace_rule_sets, &name) {
            self.replace_rules_status = Some((false, format!("没有找到规则集「{name}」")));
            cx.notify();
            return;
        }
        // 删完之后选中项夹回有效范围：列表空了就回到「未选择」，否则顶上来一条，
        // 免得下拉停在越界下标上（渲染层虽然会夹，但状态本身不该是脏的）。
        self.replace_rule_selected = if self.replace_rule_sets.is_empty() {
            None
        } else {
            Some(pos.min(self.replace_rule_sets.len() - 1))
        };
        if self.persist_replace_rule_sets().is_ok() {
            self.replace_rules_status = Some((
                true,
                format!("已删除规则集「{name}」，已同步写入 {REPLACE_RULES_FILE}"),
            ));
        }
        // 失败时 `persist_replace_rule_sets` 已把错误挂到状态行，原地保留即可。
        cx.notify();
    }

    /// 从 JSON 文件导入规则集（同名覆盖、新名追加），并立刻写回磁盘。
    ///
    /// 读盘与解析分两步：文件选择走异步对话框（同步版会起 Win32 模态循环，而 GPUI
    /// 渲染回调里正持有 RefCell 借用），读盘交给后台执行器（文件可能在网络盘上），
    /// 解析与合并回主线程做——那一步要改 `replace_rule_sets`。
    pub(crate) fn import_replace_rule_sets(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("导入替换规则集（JSON）")
                .add_filter("规则集 JSON", &["json"])
                .add_filter("所有文件", &["*"])
                .pick_file()
                .await;
            let Some(file) = handle else {
                return;
            };
            let path = file.path().to_path_buf();
            let read = cx
                .background_executor()
                .spawn(async move {
                    std::fs::read_to_string(&path)
                        .map_err(|e| format!("规则集文件读取失败（{}）：{e}", path.display()))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match read {
                    Ok(raw) => this.apply_imported_replace_rule_sets(&raw),
                    Err(err) => this.replace_rules_status = Some((false, err)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 解析一段导入的 JSON 并合并进规则集（同名覆盖），随后写回磁盘。
    ///
    /// 与文件对话框分离成同步方法：那层没法单测，这层能。
    fn apply_imported_replace_rule_sets(&mut self, raw: &str) {
        let imported = match replace_rule_sets_from_json(raw) {
            Ok(sets) => sets,
            Err(err) => {
                self.replace_rules_status = Some((false, err));
                return;
            }
        };
        if imported.is_empty() {
            self.replace_rules_status = Some((false, "文件里没有可用的规则集".to_string()));
            return;
        }
        let total = imported.len();
        let mut overwritten = 0usize;
        let mut last_pos = None;
        for set in imported {
            let existed = find_replace_rule_set(&self.replace_rule_sets, &set.name).is_some();
            // 空名条目在解析层已被跳过，这里只做兜底：失败就跳过这一条，不整批失败。
            if let Ok(pos) = upsert_replace_rule_set(&mut self.replace_rule_sets, set) {
                if existed {
                    overwritten += 1;
                }
                last_pos = Some(pos);
            }
        }
        if let Some(pos) = last_pos {
            self.replace_rule_selected = Some(pos);
        }
        if self.persist_replace_rule_sets().is_ok() {
            self.replace_rules_status = Some((
                true,
                format!(
                    "已导入 {total} 个规则集（覆盖 {overwritten} 个同名项），已写入 {REPLACE_RULES_FILE}"
                ),
            ));
        } else if let Some((_, msg)) = self.replace_rules_status.as_mut() {
            // 写盘失败时 `persist_replace_rule_sets` 已把错误挂上状态行，
            // 这里只补一句「导入的内容还在内存里」，免得用户以为整次导入白做了。
            msg.push_str("（导入的规则集仍在内存里，修好磁盘后可再点「保存」）");
        }
    }

    /// 把当前规则集导出成一份 JSON（与导入同一种扁平格式，可直接给别人导入）。
    pub(crate) fn export_replace_rule_sets(&mut self, cx: &mut Context<Self>) {
        if self.replace_rule_sets.is_empty() {
            self.replace_rules_status = Some((false, "还没有规则集可导出".to_string()));
            cx.notify();
            return;
        }
        let json = replace_rule_sets_to_json(&self.replace_rule_sets);
        let count = self.replace_rule_sets.len();
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("导出替换规则集（JSON）")
                .set_file_name(REPLACE_RULES_FILE)
                .add_filter("规则集 JSON", &["json"])
                .save_file()
                .await;
            let Some(file) = handle else {
                return;
            };
            let write_path = file.path().to_path_buf();
            let write = cx
                .background_executor()
                .spawn(async move {
                    std::fs::write(&write_path, json)
                        .map_err(|e| format!("规则集写入失败（{}）：{e}", write_path.display()))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.replace_rules_status = Some(match write {
                    Ok(()) => (true, format!("已导出 {count} 个规则集")),
                    Err(err) => (false, err),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开（或创建）术语表临时文件，供用户用记事本等编辑器编辑。
    ///
    /// 术语表是多行文本，自绘单行输入框既装不下也没法用输入法舒服编辑；改为写文件 +
    /// 系统编辑器打开，用户改完回来点「应用术语表」读回。
    pub(crate) fn open_glossary_editor(&mut self, cx: &mut Context<Self>) {
        match self.state.write_glossary_temp_file() {
            Ok(path) => {
                // 用系统默认编辑器打开（记事本 / 用户关联的 .txt 程序）
                let _ = std::process::Command::new("cmd")
                    .arg("/C")
                    .arg("start")
                    .arg("")
                    .arg(&path)
                    .spawn();
                self.glossary_status = Some(
                    "已打开软件目录下的术语表（glossary.txt）。改完保存后点「应用术语表」。"
                        .to_string(),
                );
            }
            Err(e) => {
                self.glossary_status = Some(format!("无法创建术语表文件：{e}"));
            }
        }
        cx.notify();
    }

    /// 从术语表临时文件读回内容并落盘。
    pub(crate) fn apply_glossary_from_file(&mut self, cx: &mut Context<Self>) {
        match self.state.reload_glossary_from_temp_file() {
            Ok(n) => {
                // 同样按**实际生效**数报：`glossary_prompt` 只注入前
                // `MAX_GLOSSARY_ENTRIES` 条，超限部分静默丢弃。
                // 这里说「共 N 条生效」而实际只生效 80 条就是对用户撒谎。
                let limit = self.state.config.translate.effective_glossary_limit();
                let effective = super::views::performance::effective_glossary_count(n, limit);
                if effective < n {
                    self.glossary_status = Some(format!(
                        "已应用术语表：共 {n} 条，其中 {effective} 条生效（受上限 {limit} 条限制）"
                    ));
                } else {
                    self.glossary_status = Some(format!("已应用术语表，共 {n} 条生效"));
                }
            }
            Err(e) => {
                self.glossary_status = Some(format!(
                    "读取术语表失败：{e}（请先点「编辑术语表」创建文件）"
                ));
            }
        }
        cx.notify();
    }

    /// 从磁盘导入术语表（`.txt` / `.csv`），成功后按「新增 / 覆盖 / 未变」报告。
    ///
    /// 走**异步**文件对话框（`cx.spawn` + `rfd::AsyncFileDialog`），与
    /// `MainWindow::choose_llm_model_file` 同一套写法：同步的 `FileDialog` 会起
    /// Win32 模态循环，而 GPUI 在渲染回调里正持有 RefCell 借用，模态循环会把界面
    /// 卡死。合并 / 去重 / 落盘都交给 `state.import_glossary_file`，这里只负责取
    /// 路径与落一条中性提示；文件里没解析出条目时由 `glossary_import_notice` 给出
    /// 可操作的格式提示，不静默。
    pub(crate) fn import_glossary_dialog(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("导入术语表")
                .add_filter("术语表", &["txt", "csv"])
                .add_filter("所有文件", &["*"])
                .pick_file()
                .await;
            let Some(handle) = handle else { return };
            let path = handle.path().to_path_buf();
            let _ = this.update(cx, |this, cx| {
                match this.state.import_glossary_file(&path) {
                    Ok((added, overwritten, unchanged)) => {
                        this.notice = Some(glossary_import_notice(added, overwritten, unchanged));
                    }
                    Err(e) => {
                        this.notice = Some(format!("导入术语表失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 把当前术语表导出成 CSV（默认名 `glossary.csv`）。
    ///
    /// 读取走 `state.export_glossary_csv_text`，写出复用同文件的
    /// `MainWindow::export_text_content`（另存为 + 后台写盘 + 选中文件那一条共用
    /// 出口），不另写一份写盘逻辑。空表时提前返回：导出一个只有表头的空文件没有
    /// 意义，直接给一句可操作提示。
    pub(crate) fn export_glossary_csv(&mut self, cx: &mut Context<Self>) {
        let count = self.state.glossary_entries().len();
        if count == 0 {
            self.notice = Some("术语表还是空的，先导入或编辑出条目再导出".to_string());
            cx.notify();
            return;
        }
        let content = self.state.export_glossary_csv_text();
        self.notice = Some(format!("已导出术语表（{count} 条）"));
        self.export_text_content(cx, "glossary.csv".to_string(), content, "术语表");
    }

    /// 一键把阅读速度超限的句子延长到「读得完」，只改时间、不改文本。
    ///
    /// 快照与落盘都在 `state.apply_cps_fix` 内部完成，这里只调用并按返回的
    /// `CpsFixPlan` 给出结果提示——在外面再写一次盘会把撤销栈推乱，也会让
    /// 「一键修正」变成两个数据版本的来源。空工程直接给一句可操作提示，不静默。
    pub(crate) fn apply_cps_fix_action(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            self.notice = Some("还没有字幕，先转写或导入字幕后才能修正语速".to_string());
            cx.notify();
            return;
        }
        let plan = self.state.apply_cps_fix(DEFAULT_MAX_CPS, MIN_EDIT_DUR);
        self.notice = Some(cps_fix_notice(&plan));
        cx.notify();
    }

    /// 在线翻译接口连通性自检（设置页「测试连接」按钮）
    pub(crate) fn probe_online_translate_api(&mut self, cx: &mut Context<Self>) {
        if self.is_probing_translate {
            return;
        }
        if self.state.config.translate.api_model.trim().is_empty() {
            self.translate_probe_msg = Some((false, "请先填写模型名".to_string()));
            cx.notify();
            return;
        }
        if self
            .state
            .config
            .translate
            .effective_api_key()
            .trim()
            .is_empty()
        {
            self.translate_probe_msg = Some((
                false,
                "请先填写 API Key（或设置环境变量 VOICE2WORD_API_KEY）".to_string(),
            ));
            cx.notify();
            return;
        }

        self.is_probing_translate = true;
        self.translate_probe_msg = Some((true, "正在连接在线接口...".to_string()));
        cx.notify();

        let cfg = self.state.online_translate_config();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::engines::TranslateEngine::online(cfg).probe_online() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.is_probing_translate = false;
                this.translate_probe_msg = Some(match result {
                    Ok(reply) => (true, format!("连接成功，模型回显：{}", reply.trim())),
                    Err(e) => (false, format!("连接失败：{e}")),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

// ─────────── 模型下载（首次使用引导）───────────

impl MainWindow {
    /// 后台重扫模型就位状态，结果回 UI 写入 `model_present` / `model_custom_build`。
    ///
    /// # 为什么这个任务必须离开 UI 线程（也不能塞给 `background_executor`）
    ///
    /// `AppState::refresh_model_presence` 的判定里，凡声明了 sha256 的条目都要
    /// **全文件重算哈希**（主模型几百 MB 的顺序读）——这就是任务清单里的长任务
    /// `v2w-model-presence-scan`。下载完成回调原先在 `this.update` 里同步跑它，
    /// 正好把刚转写完的界面卡住；这里把扫描移到专用线程（`utils::background_thread`，
    /// Windows 下 BELOW_NORMAL），UI 线程只做一次 map 赋值 + 通知，结果与同步
    /// 版本逐个字段一致。
    pub(crate) fn refresh_model_presence_offloaded(&mut self, cx: &mut Context<Self>) {
        // 每次请求领一个递增「代数」。连点删除 / 恢复 / 下载完成会并发跑多条全文件哈希
        // （几十秒级），先起的那条后完成时若直接回写，界面徽标会跳回它那份旧配置的快照
        // ——用户在扫描期间改的设置像被撤销了一样。用代数收口：只接受最新一代。
        let generation = self
            .model_presence_scan_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let cfg = self.state.config.clone();
        let result_rx = crate::utils::background_thread::spawn_long_task(
            "v2w-model-presence-scan",
            move || crate::utils::model_download::scan_presence(&cfg),
        );
        cx.spawn(async move |this, cx| {
            let scanned = result_rx.await;
            let _ = this.update(cx, |this, cx| {
                // 判定与回写分离：纯函数先判「这条还算不算数」，回写只做赋值。
                match scanned {
                    Ok((present, custom_build)) => {
                        if presence_scan_still_current(generation, &this.model_presence_scan_gen) {
                            this.state.model_present = present;
                            this.state.model_custom_build = custom_build;
                        }
                    }
                    // 线程起不来 / panic：退回同步扫描，结果语义不变，只是这一次
                    // 会吃 UI 线程——宁慢勿错（扫描必须发生，否则徽标停在旧态）。
                    Err(_) => this.state.refresh_model_presence(),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 下载一个模型 / 组件。
    ///
    /// 在**后台线程**执行：最大 833 MB 的文件在慢网下要几十分钟，绝不能在
    /// UI 线程上跑。进度通过 `mpsc` 回传，取消用一个 `AtomicBool`
    /// （与翻译链路同一套模式）。
    pub(crate) fn start_model_download(&mut self, item_id: &'static str, cx: &mut Context<Self>) {
        if self.state.is_downloading {
            return;
        }
        let Some(item) = crate::utils::ITEMS.iter().find(|i| i.id == item_id) else {
            self.state.download_status_msg = format!("未知的下载项：{item_id}");
            cx.notify();
            return;
        };

        self.state.is_downloading = true;
        self.state.download_current = Some(item_id.to_string());
        self.state.download_done_bytes = 0;
        self.state.download_total_bytes = 0;
        self.state.download_status_msg = format!("正在下载 {}...", item.label);
        self.model_download_cancel
            .store(false, std::sync::atomic::Ordering::SeqCst);
        cx.notify();

        let cancel = self.model_download_cancel.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, u64, String)>();

        // 进度回传协程
        cx.spawn(async move |this, cx| {
            while let Some((done, total, id)) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.state.download_done_bytes = done;
                    this.state.download_total_bytes = total;
                    this.state.download_current = Some(id);
                    cx.notify();
                });
            }
        })
        .detach();

        let item = *item;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let cb: crate::utils::model_download::ProgressFn =
                        Box::new(move |done, total, id| {
                            let _ = tx.send((done, total, id.to_string()));
                        });
                    crate::utils::model_download::download_one(&item, &cancel, Some(&cb))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.state.is_downloading = false;
                this.state.download_current = None;
                this.state.download_done_bytes = 0;
                this.state.download_total_bytes = 0;
                match result {
                    Ok(path) => {
                        info!("模型下载成功: {}", path.display());
                        // 压缩包类组件（如 llama.cpp）多一步解压，提示语要区分开，
                        // 否则用户以为只下了个包、不知道程序已经到位可用了。
                        let verb = if item.is_archive {
                            "下载并解压完成"
                        } else {
                            "下载完成"
                        };
                        this.state.download_status_msg = format!("{} {verb}", item.label);
                        if item.id == "whisper-cublas" || item.id == "whisper-cli" {
                            this.state.config.paths.whisper_cli = item.dest.to_string();
                            this.state.persist_config();
                        }
                        // 重新扫描：新文件就位后界面上的「缺失」标记要立即消失。
                        // 声明了 sha256 的条目要全文件重算哈希（主模型几百 MB），
                        // 属任务清单里的长任务 v2w-model-presence-scan：扫描放到
                        // 专用线程、结果回 UI 写缓存（见 utils::background_thread）。
                        this.refresh_model_presence_offloaded(cx);
                        // 模型换了，硬件档案里的推荐配置与预览代理可能要重算
                        this.state.refresh_hardware_recommendation();
                        // 还缺别的必需组件就接着补下一个（缺几个依次补几个）。
                        this.ensure_required_components(cx);
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        tracing::warn!(error = %err, item = item.id, "模型下载失败");
                        this.state.download_status_msg = if msg.contains("已取消") {
                            format!("{} 下载已取消", item.label)
                        } else {
                            format!("{} 下载失败：{msg}", item.label)
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 启动后的首次检查（进程级只跑一次）。
    ///
    /// 挂在首帧渲染里而不是启动流程里：下载是网络活，不能压在首帧之前；
    /// 而又必须在用户动手之前给出「正在补组件」的反馈，否则他看到的就是黑屏。
    pub(crate) fn check_required_components_once(&mut self, cx: &mut Context<Self>) {
        static CHECKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if CHECKED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.ensure_required_components(cx);
    }

    /// 必需的可执行组件（ffmpeg / whisper-cli）缺了就直接下到**配置里的相对路径**。
    ///
    /// # 判据是「能不能干活」，不是「文件在不在」
    ///
    /// `missing_required_executables` 已经把「系统 PATH 上早装好了同名命令」算作可用
    /// （见 `utils::model_download`），所以这里不会对一台已有 ffmpeg 的机器重复下几十 MB。
    /// 真正缺了才下，且落在 `item.dest` 这条相对路径上——用户不需要手改配置。
    ///
    /// # 为什么一次只下一个
    ///
    /// 下载通道（`is_downloading` + 单条进度）本来就是一次一条；下完的完成回调会再调
    /// 本函数，于是「缺几个就依次补几个」。失败则靠 [`REQUIRED_AUTO_MAX_TRIES`] 收口。
    pub(crate) fn ensure_required_components(&mut self, cx: &mut Context<Self>) {
        if self.state.is_downloading {
            // 正在下：完成回调会再进来一次。
            return;
        }
        let missing =
            crate::utils::model_download::missing_required_executables(&self.state.config);
        let Some(first) = missing.first() else {
            return;
        };
        let tries = REQUIRED_AUTO_TRIES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if tries >= REQUIRED_AUTO_MAX_TRIES {
            if tries == REQUIRED_AUTO_MAX_TRIES {
                // 到顶只提示一次：再刷同样的提示条只会把界面变成噪音。
                self.notice = Some(format!(
                    "{} 仍未就位，已停止自动下载；请到「模型与组件」里手动下载，\
                     或把它放到程序目录的 {} 位置",
                    first.label, first.dest
                ));
                cx.notify();
            }
            return;
        }
        let (id, label, dest) = (first.id, first.label, first.dest);
        tracing::info!(item = id, "必需组件缺失，自动下载到配置里的相对路径");
        self.notice = Some(format!("缺少必需组件 {label}，正在自动下载到 {dest}…"));
        cx.notify();
        self.start_model_download(id, cx);
    }
    /// 一键补齐某个分块里**缺失且必需**的条目（按体积从小到大，先让用户看到进展）。
    ///
    /// `block` 由调用方（块头那个按钮）传入：三块同页铺开之后，「下什么」不再由
    /// 全局的「当前专区」决定，而是由用户点的那一块决定。
    pub(crate) fn download_all_missing(
        &mut self,
        block: crate::ui::ModelManagerTab,
        cx: &mut Context<Self>,
    ) {
        if self.state.is_downloading {
            return;
        }
        let current_tab = block;
        // 只下**当前配置下真正必需**的组件（见 model_manager::is_item_required_now）。
        //
        // 这里刻意不用 is_item_needed_for_tab：那个函数回答的是「列表里显不显示」，
        // 模型库现在全系列出，用它会把 6 个可选档位（合计约 5 GB）一起排队下载——
        // 用户点「补齐所需」的本意是「把跑不起来的补齐」，不是「把整个模型库下满」。
        let mut pending: Vec<&'static crate::utils::DownloadItem> = crate::utils::ITEMS
            .iter()
            .filter(|i| !self.state.model_is_present(i.id))
            // 先收窄到用户点的那一块：三块同页之后，每个块头的按钮只该下自己那块
            // 的东西，否则点「公用组件」的头会把 Whisper 模型也排进队列。
            .filter(|i| {
                crate::ui::components::model_manager::is_item_needed_for_tab(
                    i.id,
                    current_tab,
                    &self.state,
                )
            })
            .filter(|i| {
                crate::ui::components::model_manager::is_item_required_now(i.id, &self.state)
            })
            .collect();
        // 小文件先下：几十 MB 的 VAD/tokens 几秒就完，用户立刻看到进展；
        // ffmpeg(50MB) 与主模型(180MB+) 排在后面。
        pending.sort_by_key(|i| i.size);
        if pending.is_empty() {
            self.state.download_status_msg = match current_tab {
                crate::ui::ModelManagerTab::SenseVoice => "SenseVoice 所需组件均已就位".to_string(),
                crate::ui::ModelManagerTab::Whisper => "Whisper 所需组件均已就位".to_string(),
                crate::ui::ModelManagerTab::Common => "公用支撑组件均已就位".to_string(),
            };
            cx.notify();
            return;
        }

        // 条目总数在循环前取好：`pending` 会被 for 消费掉，
        // 收尾提示还要用它算「完成 N/M」。
        let total = pending.len();
        let cancel = self.model_download_cancel.clone();
        cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        self.state.is_downloading = true;
        self.state.download_status_msg = format!("正在补齐 {total} 个缺失组件...");
        cx.notify();

        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, u64, String)>();
        cx.spawn(async move |this, cx| {
            while let Some((done, total_b, id)) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.state.download_done_bytes = done;
                    this.state.download_total_bytes = total_b;
                    this.state.download_current = Some(id);
                    cx.notify();
                });
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            // 逐个串行下载：并发下多个大文件抢带宽，反而都变慢，
            // 且进度条会来回跳（用户无法判断「还剩多少」）。
            let mut ok = 0usize;
            let mut failed: Vec<&'static str> = Vec::new();
            let mut last_msg = String::new();
            for item in pending {
                if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                    last_msg = "批量下载已取消".to_string();
                    break;
                }
                let tx_cb = tx.clone();
                let cancel_cb = cancel.clone();
                let item_copy = *item;
                let r = cx
                    .background_executor()
                    .spawn(async move {
                        let cb: crate::utils::model_download::ProgressFn =
                            Box::new(move |done, tot, id| {
                                let _ = tx_cb.send((done, tot, id.to_string()));
                            });
                        crate::utils::model_download::download_one(
                            &item_copy,
                            &cancel_cb,
                            Some(&cb),
                        )
                    })
                    .await;
                match r {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        tracing::warn!(item = item.id, error = %e, "批量下载中该项失败");
                        failed.push(item.label);
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.state.is_downloading = false;
                this.state.download_current = None;
                this.state.download_done_bytes = 0;
                this.state.download_total_bytes = 0;
                // 与单条目下载完成同源：重扫（含 sha256 全文件读）走专用线程，
                // 理由见 `refresh_model_presence_offloaded` 的文档注释。
                this.refresh_model_presence_offloaded(cx);
                this.state.refresh_hardware_recommendation();
                this.state.download_status_msg = if !last_msg.is_empty() {
                    last_msg
                } else if failed.is_empty() {
                    format!("全部 {total} 个组件下载完成")
                } else {
                    format!("完成 {ok}/{total}；失败：{}", failed.join("、"))
                };
                cx.notify();
            });
        })
        .detach();
    }
}

// ── 转写收尾（写库后台化）与画面预览抽帧的判定逻辑 ─────────────────────
//
// 下面这些是**纯函数 / 常量**，与 `MainWindow` 无关，所以能脱开 GPUI 单测：
// 收尾流程本身要跑 sqlite + 后台线程，不适合硬造集成测试，可判定的部分都在这里。

/// 画面预览抽帧的连续失败计数。
///
/// 为什么是进程级静态量、而不是抽帧 worker 里的局部变量：worker 在没有待处理请求
/// 时会自行退出（见 `trigger_extract_frame`），下一次拖/点时间轴会另起一个 worker——
/// 放在局部，用户「点一次失败一次、再点一次又失败」永远攒不到阈值，就只剩日志、
/// 没有可见反馈。抽到一帧即清零，所以它是**连续**失败计数。
static FRAME_FAILURE_STREAK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 连续失败到这个次数才挂一次性提示（见 [`frame_failure_notice`]）。
const FRAME_FAILURE_NOTICE_AFTER: u32 = 3;

/// 抽帧连续失败时的一次性提示文案（**纯函数**）。
///
/// 只在「刚好跨过阈值」那一次返回文案：拖动时间轴一次能触发几十次抽帧，每条失败都
/// 挂提示等于刷屏；`streak > 阈值` 返回 `None`，因为提示条上已经是同一条内容了。
/// 一旦抽帧成功，计数清零（见调用点），下次再连续失败会重新提示一次。
fn frame_failure_notice(streak: u32) -> Option<&'static str> {
    (streak == FRAME_FAILURE_NOTICE_AFTER).then_some(
        "画面预览取帧失败：多半是找不到 ffmpeg——把它放到 tools/ffmpeg.exe，
或装到系统 PATH 里（也可以在设置里填绝对路径）；字幕与导出不受影响",
    )
}

/// 一次「脏字幕落库」（原 `flush_segments_if_dirty`）的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SegmentFlushOutcome {
    /// 没有脏数据：原逻辑里 flush 是零开销 no-op，不碰 `db_write_error`。
    Skipped,
    /// 写库成功（含「尚无主键、无需写库」）：原逻辑里 flush 会把 `db_write_error` 清空。
    Saved,
    /// 写库失败：原逻辑里 flush 会挂「字幕保存失败：…」并把脏标记置回。
    Failed(String),
}

/// 把收尾里两处写库的结局折算成新的 `db_write_error`（**纯函数**）。
///
/// 顺序**必须**是「先 flush、后 insert」：反过来就会丢掉 insert 的错误——flush 成功
/// 会把 `db_write_error` 清空，先挂上的那条会被顺手吞掉，用户再也看不到「这条转写
/// 没进历史库」。拼接规则与 `AppState::record_db_write_error` 一致（「；」相连）。
fn finish_db_write_error(
    current: Option<String>,
    flush: &SegmentFlushOutcome,
    insert_error: Option<String>,
) -> Option<String> {
    let mut merged = match flush {
        SegmentFlushOutcome::Skipped => current,
        SegmentFlushOutcome::Saved => None,
        SegmentFlushOutcome::Failed(err) => Some(append_db_write_error(
            current,
            format!("字幕保存失败：{err}"),
        )),
    };
    if let Some(msg) = insert_error {
        merged = Some(append_db_write_error(merged, msg));
    }
    merged
}

/// `AppState::record_db_write_error` 的同款拼接规则：已有提示时用「；」接上，不覆盖。
fn append_db_write_error(current: Option<String>, msg: String) -> String {
    match current {
        Some(prev) => format!("{prev}；{msg}"),
        None => msg,
    }
}

/// 回写时该按哪个 flush 结局折算提示条（**纯函数**）。
///
/// 后台这段时间里用户自己的落库也可能失败、挂上**更新**的提示；那种情况下不能再用
/// 本次 flush 的「成功」把它清掉——清掉等于告诉用户「都存好了」。所以只有状态里的
/// 错误仍是派发时那一条，才原样采信 flush 的结局；否则只保留「本次 flush 也失败」
/// 这一件新事实，其余不动。
fn flush_outcome_to_apply(
    spawned_with: Option<&str>,
    current: Option<&str>,
    flush: &SegmentFlushOutcome,
) -> SegmentFlushOutcome {
    if current == spawned_with {
        return flush.clone();
    }
    if matches!(flush, SegmentFlushOutcome::Failed(_)) {
        return flush.clone();
    }
    SegmentFlushOutcome::Skipped
}

/// 延后回写要用的「文档身份」快照。
///
/// 收尾的写库被挪到后台之后，回写时状态可能已经换人了：后台这段时间（JSON 序列化 +
/// 视频指纹读盘 + 两次 SQLite 访问，几十到几百毫秒）里用户完全可能点开另一条历史
/// 工程、或又开了一单转写。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DocumentIdentity {
    /// 当前工程对应的历史库主键（本次转写的新 id 还没登记，这里记的仍是**旧**值）。
    active_task_id: Option<i64>,
    /// 当前工作台选中的媒体文件。
    selected_file: Option<PathBuf>,
}

/// 这条后台扫描的结论还能不能回写界面（**纯函数**，便于单测钉死判定）。
///
/// 只看代数：扫的是派发那一刻的配置，而「已发放的最大代数」只增不减，所以
/// `mine == current` 才说明这之后没有人再请求过扫描。旧的代数直接丢弃——
/// 它的配置快照已经被更新的请求取代，写回去会让徽标回跳（界面像被撤销了设置）。
fn presence_scan_still_current(mine: u64, current: &std::sync::atomic::AtomicU64) -> bool {
    current.load(std::sync::atomic::Ordering::SeqCst) == mine
}

/// 延后回写落地前判一次：这份结果还属不属于当前文档（**纯函数**）。
///
/// 只比 `active_task_id` 与 `selected_file`：
/// - 换工程（`load_task_with` / `load_from_cache`）两者都会变，换选片只变后者；
/// - **不**比 `segments_revision`：用户随手编辑一句也会推进它，而那种情况下跳过登记
///   会把后续「保存字幕」按旧 id 写回上一支片子——比不登记危险得多；
/// - 也**不**用 `state.status`：批量模式下一条刚结束、下一条立刻开跑，状态是
///   `Processing`，但这时登记 id 恰恰是对的（工作台就停在这条结果上）。
fn still_same_document(expected: &DocumentIdentity, current: &DocumentIdentity) -> bool {
    if current.active_task_id != expected.active_task_id {
        return false;
    }
    if current.selected_file != expected.selected_file {
        return false;
    }
    true
}

/// 「写历史库」这一步的全部输入（后台任务的入参，全是拥有所有权的数据）。
struct HistoryInsert {
    file_path: String,
    file_name: String,
    duration: f64,
    segments: Vec<Segment>,
}

/// 上一个工程待回写的脏字幕（原 `flush_segments_if_dirty` 的输入）。
struct DirtySegmentFlush {
    /// **旧**工程的 `active_task_id`（`None` 表示尚未落库、无需写库）。
    task_id: Option<i64>,
    /// 当时内存里的整份字幕。
    segments: Vec<Segment>,
    /// 派发那一刻的写序号（`AppState::next_write_seq`）。
    ///
    /// 收尾写在**后台线程**上跑，不能自己领号（`next_seq` 的单调性靠「领号只在渲染
    /// 线程」这个前提），于是带上派发时的待发号去过闸：同工程已有更新的写落库时，
    /// 这份旧字幕会被直接跳过，而不是把新内容盖掉。
    seq: u64,
}

/// 收尾后台任务的产出。
struct DeferredFinishOutcome {
    /// 本次是否有文件路径（决定要不要刷最近任务列表 / 失效「0 秒命中」缓存）。
    wrote_history: bool,
    /// 历史库写入失败提示（成功或没有路径时为 `None`）。
    insert_error: Option<String>,
    /// 待登记的 `active_task_id`（写入成功才有值；与改前一致，失败/无路径都落 `None`）。
    new_task_id: Option<i64>,
    /// 脏字幕落库的结局。
    flush: SegmentFlushOutcome,
    /// 最近任务列表（查询成功才有值，失败时保留旧列表）。
    recent_tasks: Option<Vec<crate::storage::TaskRecord>>,
}

/// 收尾后台任务的干活部分：与改前同步版**逐条对齐**的三步。
///
/// 顺序是语义的一部分，不许重排：① 写历史库（含 segments JSON 序列化与视频指纹）
/// → ② 查最近任务列表 → ③ 用**旧** `active_task_id` 回写上一个工程的脏字幕。
/// 全是阻塞 IO，所以跑在后台线程上，返回值交给回写点落状态。
fn finish_db_work(
    db: &crate::storage::Database,
    gate: &crate::app::state::SegmentWriteGate,
    insert: Option<HistoryInsert>,
    metrics: Option<&crate::core::PipelinePerformanceMetrics>,
    flush: Option<DirtySegmentFlush>,
) -> DeferredFinishOutcome {
    // 改前这三件事都包在 `if let Some(path) = file_path` 里：没有文件路径时既不写库
    // 也不刷列表，只剩 flush 可能要做。
    let wrote_history = insert.is_some();
    let mut new_task_id: Option<i64> = None;
    let mut insert_error: Option<String> = None;
    if let Some(payload) = insert {
        match db.insert_task(
            &payload.file_path,
            &payload.file_name,
            payload.duration,
            "completed",
            &payload.segments,
            metrics,
        ) {
            Ok(id) => new_task_id = Some(id),
            Err(err) => {
                tracing::warn!(error = %err, "转写结果写入历史库失败");
                insert_error = Some(format!("历史记录保存失败：{err}"));
            }
        }
    }
    // 与 `refresh_recent_tasks` 等价：只在查询成功时覆盖列表。
    let recent_tasks = if wrote_history {
        db.list_recent_tasks(50).ok()
    } else {
        None
    };
    // 与 `flush_segments_if_dirty` 等价，但**必须过写入闸**：这里是后台线程，
    // 与去抖派发的后台写 / 同步落库可能撞同一个 task。直写 `db` 会绕过按工程水位，
    // 旧字幕于是能把更新的内容盖掉（这次要堵的正是它）。
    //
    // 没有主键（尚未落库）时 `sync_segments_to_db` 直接返回 Ok，并且会清空
    // `db_write_error`——所以这里同样算「成功」。
    let flush = match flush {
        Some(payload) => match payload.task_id {
            Some(id) => match gate.write_finished_task_segments(id, payload.seq, &payload.segments)
            {
                // 被水位跳过：库里已经有同一工程更新的内容。算成功——回写提示条时
                // 不该把一份已经写完的最新内容说成「保存失败」。
                Ok(false) => SegmentFlushOutcome::Saved,
                Ok(true) => SegmentFlushOutcome::Saved,
                Err(err) => {
                    tracing::warn!(error = %err, "字幕写库失败，改动尚未持久化");
                    SegmentFlushOutcome::Failed(err.to_string())
                }
            },
            None => SegmentFlushOutcome::Saved,
        },
        None => SegmentFlushOutcome::Skipped,
    };
    DeferredFinishOutcome {
        wrote_history,
        insert_error,
        new_task_id,
        flush,
        recent_tasks,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clear_translations_with_undo, copied_translation_positions, cps_fix_notice,
        editor_export_format_string, export_spec_for, finish_db_write_error,
        flush_outcome_to_apply, frame_failure_notice, glossary_import_notice,
        has_copied_translations, is_copied_translation, presence_scan_still_current,
        replace_rule_report_message, rule_set_name_for, still_same_document, DocumentIdentity,
        SegmentFlushOutcome, IMPORT_SUBTITLE_EXTS, SUBTITLE_EXTS,
    };
    use crate::subtitle::Segment;
    use crate::ui::EditorExportFormat;
    use std::path::PathBuf;

    /// 剪辑台导出下拉里每一档**单文件**格式都必须能算出与 writer 一致的
    /// （扩展名, 对话框过滤器名）。新增 SBV / LRC / 带时间码纯文本后逐档钉住：
    /// 少一档就会出现「下拉里能选、导出却写成别的格式/别的扩展名」。
    #[test]
    fn editor_export_spec_covers_every_single_file_format() {
        let cases = [
            (EditorExportFormat::Srt, "srt", "SRT 字幕"),
            (EditorExportFormat::Ass, "ass", "ASS 特效字幕"),
            (EditorExportFormat::Txt, "txt", "TXT 纯文本"),
            (EditorExportFormat::Vtt, "vtt", "VTT 网页字幕"),
            (EditorExportFormat::Json, "json", "JSON 结构化字幕"),
            (EditorExportFormat::EbuTtD, "ttml", "EBU-TT-D 广播字幕"),
            (EditorExportFormat::NetflixTtal, "ttal", "Netflix TTAL 字幕"),
            (EditorExportFormat::Sbv, "sbv", "SBV 播放器字幕"),
            (EditorExportFormat::Lrc, "lrc", "LRC 唱词"),
            // 带时间码纯文本：文件扩展名仍是 .txt，但写盘用的格式名是 `txt-ts`
            (EditorExportFormat::TxtTs, "txt", "带时间码纯文本"),
        ];
        for (fmt, ext, label) in cases {
            let spec = export_spec_for(editor_export_format_string(fmt));
            assert_eq!(spec.0, ext, "{fmt:?} 的扩展名变了");
            assert_eq!(spec.1, label, "{fmt:?} 的对话框过滤器名变了");
        }

        // 覆盖面：`all()` 里除「非单文件」档之外，每一档都必须在这张表里出现过，
        // 否则新加的格式会静默走 `_ => "srt"`（导出成 SRT 却不报错）。
        for fmt in EditorExportFormat::all() {
            let single_file = !matches!(
                fmt,
                EditorExportFormat::JianYing
                    | EditorExportFormat::JianYingFolder
                    | EditorExportFormat::Fcpxml
                    | EditorExportFormat::PremiereXml
                    | EditorExportFormat::BurnVideo
            );
            if single_file {
                assert!(
                    cases.iter().any(|(f, _, _)| f == fmt),
                    "{fmt:?} 没有对应的导出规格断言"
                );
            }
        }
    }

    /// 上层的 `match` 已经替剪映 / FCPXML / Premiere XML / 烧录视频分流，它们不会走到
    /// 单文件导出；万一漏进来，也必须落在一个 writer 一定写得出来的格式上，
    /// 而不是 `unreachable!()` 把 UI 线程打挂。
    #[test]
    fn non_single_file_formats_degrade_to_srt_instead_of_panicking() {
        for fmt in [
            EditorExportFormat::JianYing,
            EditorExportFormat::JianYingFolder,
            EditorExportFormat::Fcpxml,
            EditorExportFormat::PremiereXml,
            EditorExportFormat::BurnVideo,
        ] {
            assert_eq!(
                editor_export_format_string(fmt),
                "srt",
                "{fmt:?} 应回落 srt"
            );
        }
    }

    /// 导入侧两份清单必须覆盖 `subtitle::reader` 真正认识的扩展名：少一项就会出现
    /// 「reader 能解析、对话框里却选不到」，或「拖进来被当成未知文件丢掉」。同时钉住
    /// 拖拽清单**刻意不含** `txt`（拖入意图不明，备忘/日志会顶掉当前工程）。
    #[test]
    fn subtitle_import_lists_cover_reader_formats() {
        use crate::subtitle::reader::SubtitleFormat;
        use std::path::Path;

        // reader 认得的全部扩展名（含 ssa / dfxp / ttal 三个别名）
        const READER_EXTS: [&str; 11] = [
            "srt", "vtt", "txt", "json", "lrc", "ass", "ssa", "ttml", "dfxp", "ttal", "sbv",
        ];
        for ext in READER_EXTS {
            assert!(
                SubtitleFormat::from_path(Path::new(&format!("x.{ext}"))).is_some(),
                "reader 不认识的扩展名：.{ext}"
            );
            assert!(
                IMPORT_SUBTITLE_EXTS.contains(&ext),
                "导入对话框过滤器漏了 .{ext}"
            );
        }

        // 拖拽清单 = 对话框清单去掉 txt（其余必须一项不落），且必须含新增的 sbv。
        for ext in SUBTITLE_EXTS {
            assert!(
                IMPORT_SUBTITLE_EXTS.contains(&ext),
                "拖拽清单里的 .{ext} 不在导入对话框清单里"
            );
        }
        assert!(!SUBTITLE_EXTS.contains(&"txt"), "拖拽清单刻意不含 txt");
        for ext in ["sbv", "lrc"] {
            assert!(SUBTITLE_EXTS.contains(&ext), "拖拽清单漏了 .{ext}");
            assert!(IMPORT_SUBTITLE_EXTS.contains(&ext), "导入清单漏了 .{ext}");
        }
    }

    /// 回归（P2）：收尾统计必须用「实际译出」口径——
    /// 引擎可能给空串/纯空白译文打上 `translation_lang`，只数标签就会把
    /// 空白显示成「翻译完成」；分母还必须排除空源句，否则会永远「部分完成」。
    #[test]
    fn translation_coverage_ignores_blank_translations_and_empty_sources() {
        let mut with_blank = Segment::new(1, 0.0, 1.0, "你好");
        with_blank.translation = Some("   ".to_string());
        with_blank.translation_lang = Some("English".to_string()); // 脏标签：有标签无译文
        let mut good = Segment::new(2, 1.0, 2.0, "世界");
        good.translation = Some("World".to_string());
        good.translation_lang = Some("English".to_string());
        let empty_source = Segment::new(3, 2.0, 3.0, "   ");
        let missing = Segment::new(4, 3.0, 4.0, "未译");
        let segs = vec![with_blank, good, empty_source, missing];
        // 分子：只有 good 算译出（空串被忽略）；分母： 3 句有源文，空源句不计。
        assert_eq!(super::translation_coverage(&segs, "English"), (1, 3));
        // 换目标语言：旧译文不能算成本轮成果（否则又会出现「换语言后显示已译出」）
        assert_eq!(super::translation_coverage(&segs, "日本語"), (0, 3));
        assert_eq!(super::translated_out_count(&segs, "English"), 1);
    }

    // ── 拖拽导入字幕（P1-A11）：路由规则与文案都要可测 ──────────────────

    /// 拖拽路由的完整真值表：媒体 / 字幕的组合各自走哪条路。
    ///
    /// 这张表就是「拖入行为」的规格说明书——媒体照旧（不影响既有队列语义），字幕
    /// 只在「恰好一个」时才导入，多了只提示、既不会静默也不会替用户挑一个。
    #[test]
    fn drop_route_covers_media_and_subtitle_combinations() {
        use super::{drop_route, DropRoute};
        assert_eq!(drop_route(0, 0), DropRoute::Nothing);
        assert_eq!(drop_route(0, 1), DropRoute::SubtitleOnly);
        assert_eq!(drop_route(0, 2), DropRoute::TooManySubtitles);
        assert_eq!(drop_route(1, 0), DropRoute::MediaOnly);
        assert_eq!(drop_route(7, 0), DropRoute::MediaOnly);
        assert_eq!(drop_route(3, 1), DropRoute::MediaAndSubtitle);
        assert_eq!(drop_route(3, 2), DropRoute::MediaAndTooManySubtitles);
    }

    /// 多个字幕的提示要说清数量和「一次一个」——只说「不支持」用户没有下一步。
    #[test]
    fn too_many_subtitles_notice_states_the_one_at_a_time_rule() {
        let msg = super::too_many_subtitles_notice(3);
        assert!(msg.contains('3'), "提示里要带上数量：{msg}");
        assert!(
            msg.contains("一次只导入一个"),
            "提示要给出下一步动作：{msg}"
        );
    }

    /// 导入提示必须把「估算出来的时间轴」说清楚：TXT 整条是合成的，LRC 只错末句。
    #[test]
    fn import_notice_flags_estimated_timelines() {
        assert!(super::import_notice("a", 3, true, false).contains("占位时间轴"));
        assert!(super::import_notice("a", 3, false, true).contains("最后一句"));
        assert_eq!(
            super::import_notice("a", 3, false, false),
            "已导入「a」3 句字幕"
        );
    }

    // ── 模型在位扫描的「代数」收口 ──────────────────────────────────────

    /// 只有最新一代的扫描结论能回写界面：先起的那条后完成时，它的配置快照已经被取代，
    /// 写回去会让徽标跳回旧态（界面像把用户的设置撤销了）。
    #[test]
    fn stale_model_presence_scan_result_is_discarded() {
        use std::sync::atomic::AtomicU64;

        let current = AtomicU64::new(1);
        assert!(presence_scan_still_current(1, &current), "同一代必须放行");

        // 又点了一次删除 / 恢复 / 下载完成：代数推进，先前那条的结论作废。
        current.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(
            !presence_scan_still_current(1, &current),
            "旧代的结论必须被丢弃，否则徽标回跳"
        );
        assert!(presence_scan_still_current(2, &current), "最新一代必须放行");

        // 连点多次仍然只认最后那一代（单调递增，不会回退）。
        current.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(!presence_scan_still_current(2, &current));
        assert!(presence_scan_still_current(3, &current));
    }

    // ── 转写收尾（写库后台化）：提示条折算与延后回写判定 ──────────────────

    /// 写历史库失败 + flush 成功：flush 成功会清空 `db_write_error`，insert 的错误必须
    /// 仍然可见——这正是改前那段注释反复强调、也最容易被重排吞掉的一条。
    #[test]
    fn finish_db_write_error_keeps_insert_error_after_flush_clears_notice() {
        let merged = finish_db_write_error(
            Some("上一次的失败".to_string()),
            &SegmentFlushOutcome::Saved,
            Some("历史记录保存失败：库被锁".to_string()),
        );
        assert_eq!(merged.as_deref(), Some("历史记录保存失败：库被锁"));
    }

    /// 两处都失败：两条都要留（flush 报的是上一个工程，insert 报的是本次结果），
    /// 顺序也必须是「先 flush、后 insert」，与改前的挂载顺序一致。
    #[test]
    fn finish_db_write_error_keeps_both_failures_in_order() {
        let merged = finish_db_write_error(
            None,
            &SegmentFlushOutcome::Failed("磁盘已满".to_string()),
            Some("历史记录保存失败：库被锁".to_string()),
        );
        assert_eq!(
            merged.as_deref(),
            Some("字幕保存失败：磁盘已满；历史记录保存失败：库被锁")
        );
    }

    /// 没有任何落库动作（不脏 + 无文件路径）时不能碰已有提示：改前 flush 是 no-op。
    #[test]
    fn finish_db_write_error_does_not_touch_notice_when_nothing_wrote() {
        let merged = finish_db_write_error(
            Some("上一次的失败".to_string()),
            &SegmentFlushOutcome::Skipped,
            None,
        );
        assert_eq!(merged.as_deref(), Some("上一次的失败"));
    }

    /// 延后回写的落地判据：文档身份一致才登记主键；换工程或换片子都必须跳过，
    /// 否则后台回写会把主键挂到别的文档上，后续「保存字幕」写错行。
    #[test]
    fn deferred_finish_lands_only_while_document_identity_matches() {
        let expected = DocumentIdentity {
            active_task_id: Some(7),
            selected_file: Some(PathBuf::from("a.mp4")),
        };
        assert!(still_same_document(&expected, &expected.clone()));
        // 用户点开了另一条历史工程
        assert!(!still_same_document(
            &expected,
            &DocumentIdentity {
                active_task_id: Some(9),
                ..expected.clone()
            }
        ));
        // 工作台换了一支片子（同一个 id 也可能：这条片子被重转过）
        assert!(!still_same_document(
            &expected,
            &DocumentIdentity {
                selected_file: Some(PathBuf::from("b.mp4")),
                ..expected.clone()
            }
        ));
    }

    /// 回写期间用户自己的落库失败挂了**更新**的提示：不能被本次 flush 的「成功」
    /// 顺手清掉（那等于告诉用户「都存好了」）。
    #[test]
    fn newer_write_error_survives_deferred_flush_success() {
        // 状态没被别人动过 → 原样采信 flush 的结局
        assert_eq!(
            flush_outcome_to_apply(Some("旧错误"), Some("旧错误"), &SegmentFlushOutcome::Saved),
            SegmentFlushOutcome::Saved
        );
        // 用户刚挂了新的错误 → 「成功」降级为「什么都不做」，那条新错误得以保留
        assert_eq!(
            flush_outcome_to_apply(
                Some("旧错误"),
                Some("字幕保存失败：磁盘已满"),
                &SegmentFlushOutcome::Saved
            ),
            SegmentFlushOutcome::Skipped
        );
        // 本次 flush 也失败 → 两条错误都要留
        assert_eq!(
            flush_outcome_to_apply(
                None,
                Some("字幕保存失败：磁盘已满"),
                &SegmentFlushOutcome::Failed("库被锁".to_string())
            ),
            SegmentFlushOutcome::Failed("库被锁".to_string())
        );
    }

    /// 抽帧连续失败的提示必须在「跨过阈值那一次」给，且文案要指向可操作的原因；
    /// 每条都挂等于刷屏，超过阈值还挂也等于刷屏。
    ///
    /// 文案钉子：实测里这条提示最常见的成因是「机器上有 ffmpeg 但没放进 tools/、
    /// 也没加进 PATH」——所以提示必须给出这两条可操作的做法，而不是只说「失败了」。
    #[test]
    fn frame_failure_notice_fires_once_per_failure_streak() {
        assert!(frame_failure_notice(0).is_none());
        assert!(frame_failure_notice(1).is_none());
        assert!(frame_failure_notice(2).is_none());
        let msg = frame_failure_notice(3).expect("连续失败到阈值必须给一次提示");
        assert!(msg.contains("ffmpeg"), "提示要指向 ffmpeg：{msg}");
        assert!(
            msg.contains("PATH") && msg.contains("tools/ffmpeg.exe"),
            "提示要给出两条可操作的做法（放进 tools/ 或加进 PATH）：{msg}"
        );
        assert!(
            msg.contains("字幕与导出不受影响"),
            "要说明影响面，避免用户以为转写结果也坏了：{msg}"
        );
        assert!(
            frame_failure_notice(4).is_none(),
            "阈值之后不再重复提示，避免刷屏"
        );
        assert!(frame_failure_notice(50).is_none(), "长期失败也只提示一次");
    }

    // ── 清空译文 / 清理复制译文（第 8、10 项）────────────────────────────

    /// 最小可用的 `AppState`（内存 SQLite + 假路径引擎，不碰显卡/磁盘/子进程），
    /// 只为验证「清空译文 / 撤销」这类纯状态逻辑。
    ///
    /// 与 `app::state` 测试里的同名助手同构：用 `HardwareProfile::cpu_only()` 而不是
    /// `AppState::new`——后者会通过 wgpu 枚举显卡，单测没必要为一份硬件档案初始化
    /// 图形栈。
    fn copied_state(segments: Vec<Segment>) -> crate::app::state::AppState {
        use crate::engines::{FFmpegEngine, HardwareProfile, LLMEngine, WhisperEngine};
        use crate::storage::Database;
        use crate::utils::AppConfig;
        use std::sync::Arc;

        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let pipeline = Arc::new(crate::core::TaskPipeline::new(
            Arc::new(FFmpegEngine::new("ffmpeg")),
            Arc::new(WhisperEngine::new("whisper-cli", "ggml-small.bin", 4, 1)),
            None,
            Arc::new(LLMEngine::new("llama-cli", "model.gguf", 2048, 4)),
            None,
        ));
        let mut state = crate::app::state::AppState::with_hardware(
            AppConfig::default(),
            db,
            pipeline,
            HardwareProfile::cpu_only(),
        );
        state.segments = segments;
        state
    }

    /// 回归（第 8 项）：清空译文的入口必须**自己**递增修订号。
    ///
    /// 修前本函数不递增、由 `editor.rs` 里那个按钮手工补一次：任何人加一个新调用点
    /// 忘了补，字幕清单就会继续按双语行高渲染、翻译卡仍说「已全部翻译」。这里钉住
    /// 「调用一次就递增一次」，并顺带验证撤销栈里拿到的是清空**之前**的快照。
    #[test]
    fn clear_translations_with_undo_bumps_segments_revision() {
        let mut first = Segment::new(1, 0.0, 1.0, "第一句");
        first.translation = Some("First".to_string());
        first.translation_lang = Some("English".to_string());
        let mut second = Segment::new(2, 1.0, 2.0, "第二句");
        second.translation = Some("Second".to_string());
        let mut state = copied_state(vec![first, second]);

        let before = state.segments_revision;
        assert!(clear_translations_with_undo(&mut state), "有译文就该清掉");
        assert_eq!(
            state.segments_revision,
            before + 1,
            "清空译文必须推进修订号（双语行高 / 译出句数 / 质检都按它失效）"
        );
        assert!(state.segments.iter().all(|s| s.translation.is_none()));
        assert!(state.segments.iter().all(|s| s.translation_lang.is_none()));

        // 撤销把两份译文原样换回来 ⇒ 快照确实取在改动之前。
        assert!(state.undo(), "清空译文必须留在撤销栈里");
        assert_eq!(state.segments[0].translation.as_deref(), Some("First"));
        assert_eq!(state.segments[1].translation.as_deref(), Some("Second"));
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("English")
        );

        // 没有译文可清时不许压撤销点（返回 false，也不该推进修订号）。
        for seg in &mut state.segments {
            seg.translation = None;
            seg.translation_lang = None;
        }
        let revision_after_clear = state.segments_revision;
        assert!(!clear_translations_with_undo(&mut state));
        assert_eq!(state.segments_revision, revision_after_clear);
    }

    /// 「译文与原文一字不差」的判定必须与翻译管线那份判据同口径（同一份实现），
    /// 包括短句 / 纯拉丁术语的放行——两处口径分家，界面与告警就会互相打脸。
    #[test]
    fn copied_translation_predicate_matches_pipeline_criterion() {
        for (source, translation, expected) in [
            (
                "今天天气很好我们出去散步吧",
                "今天天气很好我们出去散步吧",
                true,
            ),
            (
                "今天天气很好我们出去散步吧",
                " 今天天气很好我们出去散步吧 ",
                true,
            ),
            (
                "今天天气很好我们出去散步吧",
                "今天天气很好，我们出去散步吧",
                false,
            ),
            ("OK", "OK", false),
            ("Hello world", "Hello world", false),
            ("P(A+B)=1 概率", "P(A+B)=1 概率", true),
        ] {
            let mut seg = Segment::new(1, 0.0, 1.0, source);
            seg.translation = Some(translation.to_string());
            assert_eq!(
                is_copied_translation(&seg),
                expected,
                "原文「{source}」/ 译文「{translation}」的判定不符"
            );
            assert_eq!(
                is_copied_translation(&seg),
                crate::engines::llm::is_untranslated_copy(source, translation),
                "与管线判据分家了"
            );
        }
    }

    /// 位置下标（清空用）与「有没有复制句」（决定入口显不显示）两个查询必须一致，
    /// 且无译文 / 无复制句时都返回空。
    #[test]
    fn copied_translation_positions_and_probe_agree() {
        let mut copied = Segment::new(1, 0.0, 1.0, "这是一句地道的复制译文");
        copied.translation = Some("这是一句地道的复制译文".to_string());
        let mut fine = Segment::new(2, 1.0, 2.0, "这句翻译是正常的");
        fine.translation = Some("This sentence has been translated".to_string());
        let mut untranslated = Segment::new(3, 2.0, 3.0, "这句还没翻译");
        untranslated.translation = None;
        let segments = vec![copied, fine, untranslated];

        assert_eq!(copied_translation_positions(&segments), vec![0]);
        assert!(has_copied_translations(&segments));
        assert!(!has_copied_translations(&segments[1..]));
        assert!(copied_translation_positions(&[]).is_empty());
    }

    /// 规则集应用后的提示文案：处数 / 涉及句数 / 译文命中都要说清（纯函数）。
    #[test]
    fn replace_rule_report_message_summarizes_hits() {
        let report = crate::ui::ReplaceRuleReport {
            rules_hit: 2,
            text_hits: 5,
            translation_hits: 3,
            changed: vec![1, 2, 5],
        };
        let msg = replace_rule_report_message("口头禅", &report);
        assert!(msg.contains("口头禅"), "{msg}");
        assert!(msg.contains("8 处"), "总命中必须含译文命中：{msg}");
        assert!(msg.contains("3 句"), "涉及句数要说清：{msg}");
        assert!(msg.contains("译文 3 处"), "译文命中要单独点名：{msg}");

        let text_only = crate::ui::ReplaceRuleReport {
            text_hits: 1,
            changed: vec![7],
            ..Default::default()
        };
        let msg = replace_rule_report_message("术语", &text_only);
        assert!(!msg.contains("译文"), "没有命中译文时不该提译文：{msg}");
    }

    /// 规则集名字取自查找串：短串原样（去掉首尾空白），超长串截断到 20 字 + 省略号，
    /// 免得下拉列表被一条超长查找串撑爆。
    #[test]
    fn rule_set_name_for_find_text_truncates() {
        assert_eq!(rule_set_name_for("  呃  "), "规则：呃");
        assert_eq!(rule_set_name_for("transformer"), "规则：transformer");

        let long: String = "这".repeat(25);
        let name = rule_set_name_for(&long);
        assert!(name.ends_with('…'), "{name}");
        assert_eq!(
            name.chars().count(),
            "规则：".chars().count() + 20 + 1,
            "截断到 20 个字符 + 省略号：{name}"
        );
    }

    /// 术语表导入提示：空文件 / 全部未变 / 有增有改三条分支。
    #[test]
    fn glossary_import_notice_covers_all_branches() {
        assert!(
            glossary_import_notice(0, 0, 0).contains("没有条目"),
            "空文件要给可操作的格式提示"
        );
        assert_eq!(
            glossary_import_notice(0, 0, 7),
            "术语表已是最新：未变 7 条，没有新增或覆盖"
        );
        assert_eq!(
            glossary_import_notice(3, 2, 7),
            "已导入术语表：新增 3 / 覆盖 2 / 未变 7"
        );
    }

    /// CPS 修正提示：无改动 / 只延长 / 部分延不了 / 全部延不了四条分支。
    ///
    /// 「全部延不了」（`extended` 为空、`unresolved` 非空）必须与「无改动」分开：
    /// 两者 `has_changes()` 都是 false，但一个有话要说、一个是真的不用动。
    #[test]
    fn cps_fix_notice_covers_all_branches() {
        let plan = |extended: Vec<usize>, unresolved: Vec<usize>| crate::subtitle::CpsFixPlan {
            times: Vec::new(),
            extended,
            unresolved,
        };
        assert_eq!(cps_fix_notice(&plan(vec![], vec![])), "语速都在合理范围内");
        assert_eq!(
            cps_fix_notice(&plan(vec![2, 5], vec![])),
            "已延长 2 句，阅读速度回到合理范围"
        );
        let partial = cps_fix_notice(&plan(vec![2, 5], vec![5]));
        assert!(
            partial.contains("已延长 2 句") && partial.contains("另有 1 句"),
            "{partial}"
        );
        let none = cps_fix_notice(&plan(vec![], vec![4, 9]));
        assert!(
            none.contains("2 句") && none.contains("延不开"),
            "全部 unresolved 不能被当成「都在合理范围内」：{none}"
        );
    }

    #[test]
    fn speaker_rename_and_cycle_logic() {
        let mut segments = vec![
            Segment::new(1, 0.0, 1.0, "片段一"),
            Segment::new(2, 1.0, 2.0, "片段二"),
        ];
        segments[0].speaker = Some(0);
        segments[1].speaker = Some(1);

        // 批量重命名角色
        for seg in &mut segments {
            if seg.speaker == Some(0) {
                seg.speaker_name = Some("演讲人".to_string());
            }
        }
        assert_eq!(segments[0].speaker_label().as_deref(), Some("演讲人"));
        assert_eq!(segments[1].speaker_label().as_deref(), Some("说话人 2"));

        // 循环切换说话人 (0 -> 1)
        segments[0].speaker = Some((segments[0].speaker.unwrap() + 1) % 4);
        assert_eq!(segments[0].speaker, Some(1));
    }
}
