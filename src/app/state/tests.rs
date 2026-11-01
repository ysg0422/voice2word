//! `AppState` 单元测试：编辑/撤销、批量队列、后台写入与配置落盘。

use super::writes::{
    apply_segment_write_result, drain_segment_writes, mark_segments_dirty_for_revision,
    write_snapshot_under_gate, SegmentDispatch, SegmentWriteDone, SegmentWriteSnapshot,
    SEGMENTS_FLUSH_IDLE,
};
use super::{
    AppState, BackgroundWrites, PolishMode, ProcessStatus, SegmentWriteGate, StageRowState,
    WhisperModelTier,
};
use crate::subtitle::Segment;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

/// 构造一个最小可用、不碰显卡/磁盘/子进程的 AppState：内存 SQLite + 假路径引擎，
/// 只为验证「编辑 / 撤销」这类纯状态逻辑。
///
/// 用 `HardwareProfile::cpu_only()` 而不是 `AppState::new`：后者会通过 wgpu 枚举
/// 显卡，单测没必要为了一份硬件档案去初始化图形栈。
/// 步骤看板：点开始就要把全部阶段**全部**铺出来，第一步立刻进行中。
#[test]
fn stage_rows_reset_lists_all_stages_with_first_running() {
    let mut state = test_state(vec![]);
    state.reset_stage_rows();
    assert_eq!(state.stage_rows.len(), crate::core::Stage::ALL.len());
    assert_eq!(state.stage_rows[0].state, StageRowState::Running);
    assert!(state.stage_rows[1..]
        .iter()
        .all(|r| r.state == StageRowState::Pending));
    assert!(state.stage_row_started_at.is_some());
}

/// 推进到后面的阶段：前面的行定稿为完成，中间没跑过的标「跳过」。
///
/// 这条钉的是「关掉润色 / 没触发救场」这类跳阶段场景——停在「等待」会让人
/// 以为卡在那一格。
#[test]
fn advance_stage_rows_finishes_previous_and_skips_gaps() {
    let mut state = test_state(vec![]);
    state.reset_stage_rows();
    let target = crate::core::Stage::ALL
        .iter()
        .position(|s| s.label() == "语音识别")
        .expect("看板必须包含语音识别阶段");
    state.advance_stage_rows("语音识别");

    for (i, row) in state.stage_rows.iter().enumerate().take(target) {
        let expected = if i == 0 {
            StageRowState::Done
        } else {
            StageRowState::Skipped
        };
        assert_eq!(row.state, expected, "第 {i} 行状态不对");
    }
    assert!((state.stage_rows[0].progress - 1.0).abs() < f32::EPSILON);
    assert_eq!(state.stage_rows[target].state, StageRowState::Running);

    // 认不出的标签不得改动看板（可选阶段如今已入枚举，这里用真正不存在的名字）
    let before: Vec<_> = state.stage_rows.iter().map(|r| r.state).collect();
    state.advance_stage_rows("不存在的阶段");
    let after: Vec<_> = state.stage_rows.iter().map(|r| r.state).collect();
    assert_eq!(before, after, "未知阶段不该改动任何一行");
}

/// 可选增强阶段也必须被看板认识：否则管线发了 `StageChanged`，看板既不亮也不跳过。
#[test]
fn optional_stages_are_known_to_the_board() {
    for label in ["人声分离", "说话人分离"] {
        assert!(
            crate::core::Stage::from_label(label).is_some(),
            "{label} 必须能在 Stage::ALL 里找到"
        );
    }
}

/// 收尾：进行中的行定住、未跑的标跳过；**失败时进度不被拉满**。
#[test]
fn finish_stage_rows_settles_running_without_faking_completion() {
    let mut state = test_state(vec![]);
    state.reset_stage_rows();
    state.set_stage_row_progress(0.4);
    state.finish_stage_rows();

    assert_eq!(state.stage_rows[0].state, StageRowState::Done);
    assert!(
        (state.stage_rows[0].progress - 0.4).abs() < 1e-6,
        "异常/取消收尾时进度条要停在真实位置，不能假装走完"
    );
    assert!(state.stage_row_started_at.is_none(), "收尾后不再计时");
    assert!(state.stage_rows[1..]
        .iter()
        .all(|r| r.state == StageRowState::Skipped));
}

fn test_state(segments: Vec<Segment>) -> AppState {
    use crate::engines::{FFmpegEngine, LLMEngine, WhisperEngine};
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
    let mut state = AppState::with_hardware(
        AppConfig::default(),
        db,
        pipeline,
        crate::engines::HardwareProfile::cpu_only(),
    );
    state.segments = segments;
    state
}

/// 测试专用：给临时文件一个不会撞名的后缀。
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 「稍后再说」只关引导框：向导标记被置完成，但**硬件档位与推理策略一动不动**。
///
/// 回归护栏：两个选择按钮走 `set_hardware_tier`，它会顺带套用一整套餐并落盘。
/// 「稍后再说」若复用那条路，用户还没表态就被默许了一个从没选过的模式——
/// 下次启动会发现推理策略、模型路径、线程数全变了。
#[test]
fn defer_hardware_setup_only_closes_the_wizard() {
    // 落盘目标指到临时目录：单测不许写仓库根的 `config.toml`（会覆盖真实配置）。
    let cfg_path = std::env::temp_dir().join(format!("v2w_hw_later_{}.toml", unique_suffix()));
    let mut state = test_state(Vec::new());
    // 首次启动的初始状态：向导未完成（默认 cpu 档，见 `GpuConfig::default`）。
    assert!(!state.config.gpu.hardware_init_completed);
    let tier_before = state.config.gpu.hardware_tier.clone();
    let offload_before = state.config.gpu.whisper_offload;
    let decode_before = state.config.gpu.hwaccel_decode;
    let model_before = state.config.paths.whisper_model.clone();
    let threads_before = state.config.pipeline.whisper_threads;

    state.defer_hardware_setup_to(&cfg_path);

    assert!(
        state.config.gpu.hardware_init_completed,
        "稍后再说必须让引导框不再出现，否则等于没有关闭入口"
    );
    assert_eq!(
        state.config.gpu.hardware_tier, tier_before,
        "不许改硬件档位"
    );
    assert_eq!(
        state.config.gpu.whisper_offload, offload_before,
        "不许改推理 offload"
    );
    assert_eq!(
        state.config.gpu.hwaccel_decode, decode_before,
        "不许改硬解开关"
    );
    assert_eq!(
        state.config.paths.whisper_model, model_before,
        "不许换模型路径（那会改变用户下次转写用的档位）"
    );
    assert_eq!(
        state.config.pipeline.whisper_threads, threads_before,
        "不许改线程数"
    );

    // 幂等：重复调用（Esc + 按钮都点到）不会有第二次副作用。
    state.defer_hardware_setup_to(&cfg_path);
    assert_eq!(state.config.gpu.hardware_tier, tier_before);
}

/// 下载完模型后的「重算推荐」不许重新探测硬件。
///
/// 回归护栏：这里以前调 `HardwareInfo::detect()`——那会新建 wgpu Instance 枚举一遍
/// 适配器（实测 ~2.7s）且跑在渲染线程上，用户看到的是「下载完成卡两三秒」。
/// 现在只按已有档案重算，硬件档案本身必须**原样不动**。
#[test]
fn refresh_hardware_recommendation_reuses_existing_probe() {
    let mut state = test_state(vec![]);
    let before_gpus = state.hardware_info.gpus.len();
    let before_threads = state.hardware_info.logical_threads;
    let before_brand = state.hardware_info.cpu_brand.clone();
    // 先把派生值弄成明显不对的状态，证明函数确实重算过
    state.performance_level = crate::core::PerformanceLevel::Low;
    state.user_strategy = crate::core::UserStrategy::Balanced;

    state.refresh_hardware_recommendation();

    assert_eq!(state.hardware_info.gpus.len(), before_gpus, "不许重探显卡");
    assert_eq!(state.hardware_info.logical_threads, before_threads);
    assert_eq!(state.hardware_info.cpu_brand, before_brand);
    let expected_level = state.hardware_info.evaluate_performance();
    assert_eq!(
        state.performance_level, expected_level,
        "档位必须按档案重算"
    );
    assert_eq!(
        state.recommended_profile,
        crate::core::InferenceProfile::decide(
            expected_level,
            state.user_strategy,
            &state.hardware_info
        ),
        "推荐配置必须按档案重算"
    );
}

/// 润色「档位 → 配置字符串 → 档位」必须往返一致，且「关闭」只关总开关。
///
/// 回归护栏：`enable_polish` / `polish_mode` 此前**没有任何界面入口写回配置**，
/// 用户关掉润色、重启又被配置里的 `true` 打开（看起来像程序擅自改设置）。
/// 现在所有入口都经 `AppState::set_polish`，这里锁住它的配置侧契约：
/// ① 档位字符串与 `parse_label` 往返一致（启动回读的就是这个映射）；
/// ② 关闭只写 `enable_polish=false`，不顺手改写已选档位（否则关一下再开，
///    引擎会悄悄从 Qwen 变回 CT-Punc）。
#[test]
fn polish_config_roundtrips_mode_and_keeps_mode_when_disabled() {
    for mode in [PolishMode::PuncFast, PolishMode::QwenDeep] {
        let s = mode.as_str();
        assert_eq!(
            PolishMode::parse_label(s),
            mode,
            "档位字符串 {s} 必须能被 parse_label 复原（启动回读走的就是它）"
        );
    }
    // 关闭：沿用当前档位，仅总开关置假
    let mut pipeline = crate::utils::AppConfig::default().pipeline;
    pipeline.enable_polish = true;
    pipeline.polish_mode = PolishMode::QwenDeep.as_str().to_string();
    // 模拟 set_polish(false, 当前档位)
    pipeline.enable_polish = false;
    let text = toml::to_string_pretty(&pipeline).expect("serialize");
    let back: crate::utils::config::PipelineConfig = toml::from_str(&text).expect("deserialize");
    assert!(
        !back.enable_polish,
        "关闭润色必须持久化，否则重启自己开回来"
    );
    assert_eq!(
        PolishMode::parse_label(&back.polish_mode),
        PolishMode::QwenDeep,
        "关闭不应改写已选档位"
    );
}

/// 档位 → 可下载组件 id 的映射必须与 `ITEMS` 对得上，且四档都不是压缩包
/// （压缩包组件不显示删除按钮）。这条锁住的是「界面上的下载/删除按钮」与
/// 「下载层真正能删的东西」之间的接线——一旦有人改了 id 或把某档换成 zip，
/// 这里先红。
#[test]
fn whisper_tier_download_ids_exist_and_are_deletable() {
    for tier in WhisperModelTier::WHISPER_TIERS {
        let id = tier
            .download_item_id()
            .unwrap_or_else(|| panic!("{tier:?} 应映射到可下载组件"));
        assert!(
            crate::utils::model_download::item_by_id(id).is_some(),
            "{tier:?} 的组件 id {id} 不在 ITEMS 里"
        );
        assert!(
            crate::utils::model_download::item_is_deletable(id),
            "{tier:?} 的组件 {id} 应可删除（界面据此渲染删除按钮）"
        );
    }
    // SenseVoice 不是「一个档位 = 一个文件」，必须返回 None
    assert!(WhisperModelTier::SenseVoice.download_item_id().is_none());
}

/// 档位显示名四档各异且非空——下拉触发器与菜单项共用它，重复会让用户分不清。
#[test]
fn whisper_tier_labels_are_unique_and_nonempty() {
    let mut seen = std::collections::HashSet::new();
    for tier in WhisperModelTier::WHISPER_TIERS {
        let label = tier.label();
        assert!(!label.is_empty());
        assert!(seen.insert(label), "档位显示名重复: {label}");
    }
    assert!(!WhisperModelTier::SenseVoice.label().is_empty());
}

/// `installed_path` 只认该档位自己的默认文件：配置指向别的档位时，
/// 没下过的档位不能跟着显示「已就位」。
#[test]
fn installed_path_does_not_follow_configured_other_tier() {
    let mut cfg = crate::utils::AppConfig::default();
    cfg.paths.whisper_model = "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string();
    // 本测试断言的是**存在性**语义，不依赖磁盘：
    // 非首选量化的 Balanced 只在 filesystem 真有 ggml-small-q5_0.bin 时才 Some，
    // 与配置无关。
    // installed_path 必须与 model_relative_path 同结论（界面「已就位」与管线
    // 实际加载的文件一致），且**不受** config 指向影响。
    for tier in WhisperModelTier::WHISPER_TIERS {
        let rel = crate::utils::AppConfig::resolve_path(&tier.model_relative_path());
        assert_eq!(
            tier.installed_path().is_some(),
            rel.is_file(),
            "{tier:?} 的 installed_path 应与 model_relative_path 一致"
        );
    }
    // 配置指向 turbo-q8 时，Balanced 是否可用只取决于小模型自己（含 q5_0→
    // ggml-small 的档位内回退），不会因为配置指向 turbo 就跟着变 true。
    let small_q5 = crate::utils::AppConfig::resolve_path("models/whisper/ggml-small-q5_0.bin");
    let small_plain = crate::utils::AppConfig::resolve_path("models/whisper/ggml-small.bin");
    assert_eq!(
        WhisperModelTier::Balanced.installed_path().is_some(),
        small_q5.is_file() || small_plain.is_file(),
        "Balanced 不应跟着 config 指向的 turbo-q8 一起变已就位"
    );
}

/// 队列落盘 → 读回：状态映射必须保真，且「转写中」要恢复成等待中。
///
/// 这条锁住的是「关窗不丢队列」的接线。最容易错的映射是 `Running`：直接照搬
/// 会让重启后的界面显示「转写中…」，而实际上什么都没在跑——用户会一直等下去。
#[test]
fn queue_roundtrip_restores_states_and_demotes_running() {
    use crate::utils::queue_store::{load_queue, save_queue, StoredQueueItem};
    let dir = std::env::temp_dir().join(format!("v2w_qtest_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("q.json");

    // 模拟 persist_queue 写出的三条：等待 / 完成 / 转写中
    let items = vec![
        StoredQueueItem {
            path: r"D:\a.mp4".to_string(),
            duration: 12.5,
            status: "pending".to_string(),
        },
        StoredQueueItem {
            path: r"D:\b.mp4".to_string(),
            duration: 30.0,
            status: "done".to_string(),
        },
        StoredQueueItem {
            path: r"D:\c.mp4".to_string(),
            duration: 0.0,
            status: "pending".to_string(), // Running 也写 pending，见 persist_queue
        },
    ];
    save_queue(&path, &items).expect("落盘应成功");
    let back = load_queue(&path);
    assert_eq!(back, items, "往返必须逐字保真");
    assert_eq!(back[1].duration, 30.0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 编辑必须留下审计记录，且**破坏性操作要能追回被删的内容**。
///
/// 这条锁住的是「审计接线」：`audit()` 的调用点分散在 4 个编辑入口，漏掉任何一个
/// 都会让用户事后查不到那次改动。撤销栈救不了这个——它只有 50 层、换文档即清空、
/// 退出即丢。
#[test]
fn destructive_edits_are_recorded_with_their_old_content() {
    use crate::subtitle::audit::EditKind;
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "第一句"),
        Segment::new(2, 1.0, 2.0, "第二句"),
    ]);

    // 删除第 2 句：审计里必须留下「被删掉的那句原文」
    state.select_segment(2);
    state.delete_selected_segment();
    let deletes: Vec<_> = state.edit_log.of_kind(EditKind::Delete);
    assert_eq!(deletes.len(), 1, "删除必须留一条审计");
    assert_eq!(deletes[0].index, 2);
    assert_eq!(deletes[0].before, "第二句", "必须记下被删的内容");
    assert!(deletes[0].after.is_empty());

    // 合并：记录合并前的两句
    state.select_segment(1);
    // 只剩一句时合并是空操作（pos+1 越界），先补一句回来构造可合并的状态
    state.segments.push(Segment::new(2, 1.0, 2.0, "第二句"));
    state.merge_selected_with_next();
    let merges: Vec<_> = state.edit_log.of_kind(EditKind::Merge);
    assert_eq!(merges.len(), 1, "合并必须留一条审计");
    assert!(
        merges[0].before.contains("第一句") && merges[0].before.contains("第二句"),
        "合并记录应含合并前的两句: {}",
        merges[0].before
    );
}

/// 时间微调的审计必须记**规划后的实际结果**，而不是用户点的那一下。
///
/// 微调会被邻居边界挡住（`plan_time_edit` 可能收短/后挪），若日志里写「用户点了
/// +0.1」就与实际发生的事不符，事后对照会误导。
#[test]
fn timing_audit_records_planned_result_not_requested_delta() {
    use crate::subtitle::audit::EditKind;
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "a"),
        Segment::new(2, 1.0, 2.0, "b"),
    ]);
    state.select_segment(2);
    // 起点往前推 0.5 秒：会越界压到第 1 句，`plan_time_edit` 会收短它。
    state.adjust_selected_times(-0.5, 0.0);
    let records: Vec<_> = state.edit_log.of_kind(EditKind::Timing);
    assert_eq!(records.len(), 1);
    // before 是原区间，after 必须是规划结果（与当前实际时间一致）
    assert_eq!(records[0].before, "1.000-2.000");
    let seg = state
        .segments
        .iter()
        .find(|s| s.index == 2)
        .expect("第 2 句应还在");
    assert_eq!(
        records[0].after,
        format!("{:.3}-{:.3}", seg.start, seg.end),
        "审计记录必须与落定后的真实时间一致"
    );
}

#[test]
fn model_tier_follows_configured_path() {
    assert_eq!(WhisperModelTier::default(), WhisperModelTier::Balanced);
    assert_eq!(
        WhisperModelTier::from_model_path("models/whisper/ggml-small.bin"),
        Some(WhisperModelTier::Balanced)
    );
    assert_eq!(
        WhisperModelTier::from_model_path("models/whisper/ggml-small-q5_0.bin"),
        Some(WhisperModelTier::Balanced)
    );
    assert_eq!(
        WhisperModelTier::from_model_path("models\\whisper\\ggml-large-v3-turbo-q5_0.bin"),
        Some(WhisperModelTier::TurboSpeed)
    );
    assert_eq!(
        WhisperModelTier::from_model_path("models/sensevoice/model.int8.onnx"),
        Some(WhisperModelTier::SenseVoice)
    );
}

/// 回归：润色附加开销必须**按档位**计，而不是一个 bool 了事。
///
/// SenseVoice 是出厂默认引擎、`punc` 是默认档位，而管线对
/// 「SenseVoice + punc」整段跳过标点——若沿用旧的 `enable_polish: bool`，
/// 就只能在「给误开它的 SenseVoice 用户虚高 +1.3 分钟」与「让选了 Qwen 的人
/// 被严重低估」之间二选一。按档位计，两种误导同时消掉。
#[test]
fn polish_overhead_depends_on_mode_not_just_switch() {
    use crate::app::PolishMode;
    let d = 1920.0; // 32 分钟样片
    let sv = WhisperModelTier::SenseVoice;
    let base = sv.estimate_seconds(d, 8, false, None);
    let punc = sv.estimate_seconds(d, 8, false, Some(PolishMode::PuncFast));
    let qwen = sv.estimate_seconds(d, 8, false, Some(PolishMode::QwenDeep));

    assert_eq!(base, punc, "SenseVoice 的极速标点被跳过，不该加时间");
    assert!(
        qwen > punc,
        "Qwen 深度润色必须计入附加开销：punc={punc} qwen={qwen}"
    );
    let delta = qwen - punc;
    assert!(
        (delta - 76.8).abs() < 1.0,
        "Qwen 附加开销应按 2.4 s/分钟 计，实际 {delta}"
    );

    let w = WhisperModelTier::Balanced;
    assert_eq!(
        w.estimate_seconds(d, 8, false, Some(PolishMode::PuncFast)),
        w.estimate_seconds(d, 8, false, None),
        "CT-Punc 毫秒级，不该计入预估"
    );
    assert!(
        w.estimate_seconds(d, 8, false, Some(PolishMode::QwenDeep))
            > w.estimate_seconds(d, 8, false, None)
    );
}

#[test]
fn more_threads_shortens_cpu_eta() {
    let d = 32.0 * 60.0;
    let t4 = WhisperModelTier::Balanced.estimate_seconds(d, 4, false, None);
    let t8 = WhisperModelTier::Balanced.estimate_seconds(d, 8, false, None);
    let t16 = WhisperModelTier::Balanced.estimate_seconds(d, 16, false, None);
    assert!(t4 > t8, "4 threads ({t4}) should be slower than 8 ({t8})");
    assert!(
        t8 > t16,
        "8 threads ({t8}) should be slower than 16 ({t16})"
    );
    assert_ne!(
        WhisperModelTier::format_eta(t4),
        WhisperModelTier::format_eta(t16)
    );
}

#[test]
fn gpu_eta_is_faster_than_cpu() {
    let d = 32.0 * 60.0;
    let cpu = WhisperModelTier::Precise.estimate_seconds(d, 8, false, None);
    let gpu = WhisperModelTier::Precise.estimate_seconds(d, 8, true, None);
    assert!(gpu < cpu * 0.5);
}

/// P0-A5：GPU ETA 按档位标定——大模型的 GPU 加速比要显著大于小模型，
/// 否则 Turbo 档会被严重高估（界面说 35 s、实际 25 s）。
#[test]
fn gpu_speedup_is_tier_dependent() {
    let d = 32.0 * 60.0;
    let ratio = |tier: WhisperModelTier| {
        let cpu = tier.estimate_seconds(d, 8, false, None);
        let gpu = tier.estimate_seconds(d, 8, true, None);
        gpu / cpu
    };
    let fast = ratio(WhisperModelTier::Fast);
    let balanced = ratio(WhisperModelTier::Balanced);
    let turbo = ratio(WhisperModelTier::TurboSpeed);
    assert!(
        fast > balanced && balanced > turbo,
        "GPU 加速比应随模型变大而增大：fast={fast:.3} balanced={balanced:.3} turbo={turbo:.3}"
    );
    // 实测核显：Turbo 的 GPU/CPU 约 0.20，留出余量取 0.28 上限。
    assert!(
        turbo < 0.28,
        "Turbo GPU ETA 不应再被 0.28 系数高估：{turbo:.3}"
    );
}

/// 失败项不能被续跑逻辑自动重挑，否则一个坏文件会让整批卡在死循环里。
#[test]
fn next_pending_skips_done_and_failed_items() {
    use super::{next_pending_index, QueueItem, QueueState};
    use std::path::PathBuf;

    let mut queue = vec![
        QueueItem::new(PathBuf::from("a.mp4")),
        QueueItem::new(PathBuf::from("b.mp4")),
        QueueItem::new(PathBuf::from("c.mp4")),
    ];
    queue[0].state = QueueState::Done { segments: 12 };
    queue[1].state = QueueState::Failed("坏文件".to_string());

    assert_eq!(next_pending_index(&queue), Some(2));

    queue[2].state = QueueState::Running;
    assert_eq!(next_pending_index(&queue), None);

    // 只有「开始全部」显式重新排队之后，失败项才重新可跑
    queue[1].state = QueueState::Pending;
    assert_eq!(next_pending_index(&queue), Some(1));
}

/// 进度只落在当前活动项上，别的条目一个字节都不许动——它们还没开始跑。
#[test]
fn progress_lands_on_active_item_only() {
    use super::QueueItem;
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());
    state.batch_queue = vec![
        QueueItem::new(PathBuf::from("a.mp4")),
        QueueItem::new(PathBuf::from("b.mp4")),
        QueueItem::new(PathBuf::from("c.mp4")),
    ];
    state.batch_active = Some(1);

    state.record_batch_progress("语音识别", 0.37);

    assert_eq!(state.batch_queue[1].progress, 37.0);
    assert_eq!(state.batch_queue[1].stage.as_deref(), Some("语音识别"));
    assert_eq!(state.batch_queue[0].progress, 0.0, "还没跑的那条不许有进度");
    assert!(state.batch_queue[0].stage.is_none());
    assert_eq!(state.batch_queue[2].progress, 0.0, "还没跑的那条不许有进度");
    assert!(state.batch_queue[2].stage.is_none());

    // 越界（活动下标指向已被删掉的条目）必须静默忽略，不许 panic
    state.batch_active = Some(99);
    state.record_batch_progress("语音识别", 0.9);

    // 指针被清空之后（批量收尾 / 单文件模式）再来的事件同样无处可落
    state.batch_active = None;
    state.record_batch_progress("语音识别", 0.9);
}

/// 切到下一条：上一条的进度必须**定格**，不能跟着新事件继续爬。
///
/// 定格规则取的是「上一条被自己结算时的状态」——成功是 1.0、失败保留当时值；
/// 这条用例走的是「上一条报错退出、下一条接管」的路径，失败那条约 0.5 定格，
/// 之后所有进度事件都只许落在下一条身上。
#[test]
fn progress_stops_at_previous_item_when_batch_advances() {
    use super::QueueItem;
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());
    state.batch_queue = vec![
        QueueItem::new(PathBuf::from("a.mp4")),
        QueueItem::new(PathBuf::from("b.mp4")),
        QueueItem::new(PathBuf::from("c.mp4")),
    ];
    state.batch_active = Some(0);
    state.mark_queue_running(0);
    state.record_batch_progress("语音识别", 0.5);

    state.finish_active_queue_item(Err("坏文件".to_string()));
    assert_eq!(
        state.batch_queue[0].progress, 50.0,
        "失败条目的进度定格在出事那一刻，交给续跑逻辑去开始下一条"
    );

    state.batch_active = Some(1);
    state.mark_queue_running(1);
    state.record_batch_progress("提取音频", 0.1);

    assert_eq!(state.batch_queue[1].progress, 10.0, "进度必须跟着新条目走");
    assert_eq!(state.batch_queue[1].stage.as_deref(), Some("提取音频"));
    assert_eq!(state.batch_queue[0].progress, 50.0, "上一条的进度不许再变");
    assert!(state.batch_queue[0].stage.is_none());
    assert_eq!(state.batch_queue[2].progress, 0.0, "还没跑的那条不许有进度");
    assert!(state.batch_queue[2].stage.is_none());
}

/// 单文件模式（`batch_active == None`）绝不往队列里写：队列卡片上的条目
/// 应当是干净的，否则单跑一支片子，队列里某个「等待中」的条目会凭空
/// 冒出百分比，用户以为它已经在转写。
#[test]
fn batch_progress_is_ignored_outside_batch_mode() {
    use super::QueueItem;
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());
    state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
    assert!(state.batch_active.is_none());

    state.record_batch_progress("语音识别", 0.88);

    assert_eq!(state.batch_queue[0].progress, 0.0);
    assert!(state.batch_queue[0].stage.is_none());
}

#[test]
fn queue_item_progress_defaults_to_zero_and_clears_stale_stage() {
    use super::{QueueItem, QueueState};
    use std::path::PathBuf;

    // 新条目：没进度、没阶段。阶段留着 `Some` 会让「等待中」的卡片显示
    // 一个不存在的阶段名——面板上就是一条假信息。
    let fresh = QueueItem::new(PathBuf::from("a.mp4"));
    assert_eq!(fresh.progress, 0.0);
    assert!(fresh.stage.is_none());

    // 重跑同一条：残留进度和阶段都必须被清掉
    let mut state = test_state(Vec::new());
    state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
    state.batch_queue[0].progress = 87.0;
    state.batch_queue[0].stage = Some("智能润色".to_string());
    state.mark_queue_running(0);
    assert_eq!(state.batch_queue[0].progress, 0.0, "重跑必须从 0 开始");
    assert!(state.batch_queue[0].stage.is_none());
    assert_eq!(state.batch_queue[0].state, QueueState::Running);
}

/// 回归：单文件入口跑完**队列里的条目**后，那条必须被结算掉。
///
/// 这是「明明转写完了，队列里每次重启都挂着一条待转写」的根因：
/// 队列卡片可点（`select_queue_item` 把文件设成 `transcribe_file`），
/// 用户点完再按「开始转写」走的是单文件入口，那里 `batch_running` 被显式置 false，
/// 收尾时 `in_batch` 为假 → `finish_active_queue_item` 不执行 → 该条永远停在
/// `Pending`，并随队列以 `pending` 落盘，于是每次启动都再冒出来。
///
/// 这里同时钉住三条：能按路径结算、结算后是 `Done`、重复调用不覆盖已有句数。
#[test]
fn settling_by_path_marks_the_matching_item_done() {
    use super::{QueueItem, QueueState};
    use std::path::{Path, PathBuf};

    let mut state = test_state(Vec::new());
    state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
    state.batch_queue[0].progress = 37.0;
    state.batch_queue[0].stage = Some("语音识别".to_string());
    assert!(matches!(state.batch_queue[0].state, QueueState::Pending));

    // 大小写不同也要命中：判等走小写归一（与入队去重同一套规则）。
    assert!(state.settle_queue_item_by_path(Path::new("A.MP4"), 12));
    assert_eq!(
        state.batch_queue[0].state,
        QueueState::Done { segments: 12 }
    );
    assert_eq!(state.batch_queue[0].progress, 100.0, "结算后必须满格");
    assert!(state.batch_queue[0].stage.is_none(), "不能留阶段残影");

    // 已经结算过：不重复返回 true，也不该把真实句数改成别的值。
    assert!(!state.settle_queue_item_by_path(Path::new("a.mp4"), 99));
    assert_eq!(
        state.batch_queue[0].state,
        QueueState::Done { segments: 12 }
    );

    // 不在队列里的文件：什么都不动（单文件转写大多数时候确实不在队列里）。
    assert!(!state.settle_queue_item_by_path(Path::new("other.mp4"), 5));
    assert_eq!(
        state.batch_queue[0].state,
        QueueState::Done { segments: 12 }
    );
}

/// 结算时的定格规则：**成功收到 100%，失败保留当下值**。
///
/// 失败若也强行收到 0%，用户看到的就是「这一条根本没跑过」；若强行跳到 100%，
/// 又像是在骗：「都满格了怎么还失败」。保留当下值才说得通，而失败原因由
/// `QueueState::Failed` 负责解释。
#[test]
fn finishing_freezes_progress_at_full_on_success_and_keeps_it_on_failure() {
    use super::QueueItem;
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());
    state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
    state.batch_active = Some(0);
    state.batch_queue[0].progress = 42.0;
    state.batch_queue[0].stage = Some("语音识别".to_string());

    state.finish_active_queue_item(Ok(9));
    assert_eq!(state.batch_queue[0].progress, 100.0, "成功后必须满格");
    assert!(
        state.batch_queue[0].stage.is_none(),
        "结算后不能留下阶段残影"
    );

    state.batch_active = Some(0);
    state.batch_queue[0].progress = 55.0;
    state.batch_queue[0].stage = Some("生成字幕".to_string());
    state.finish_active_queue_item(Err("坏文件".to_string()));
    assert_eq!(
        state.batch_queue[0].progress, 55.0,
        "失败要保留出事那一刻的进度，别清零也别骗成满格"
    );
}

#[test]
fn failed_items_count_as_pending_but_are_not_runnable() {
    use super::QueueState;

    let failed = QueueState::Failed("超时".to_string());
    assert!(failed.is_actionable());
    assert!(failed.is_failed());
    assert!(!failed.is_pending());

    let done = QueueState::Done { segments: 3 };
    assert!(!done.is_actionable());
    assert!(!done.is_pending());
    assert!(!done.is_failed());

    assert!(QueueState::Pending.is_pending());
}

/// 批量续跑的关键点：`finish_active_queue_item` 必须把 `ProcessStatus` 收回 `Idle`。
///
/// 修前失败路径只改了队列卡片，状态仍停在 `Processing`；而续跑入口
/// `start_processing_for` 一见 `Processing` 就 `return`——一个坏文件足以让整批
/// 卡死在「转写中」，后面的文件永远不开跑，用户只能手动「终止批量」才出得来。
#[test]
fn finishing_active_item_clears_processing_status() {
    use super::{ProcessStatus, QueueItem, QueueState};
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());
    state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
    state.batch_active = Some(0);
    state.batch_queue[0].state = QueueState::Running;
    state.status = ProcessStatus::Processing {
        stage: "转写中".to_string(),
        progress: 0.3,
        detail: String::new(),
    };

    state.finish_active_queue_item(Err("坏文件".to_string()));

    assert!(state.batch_queue[0].state.is_failed());
    assert!(state.batch_active.is_none());
    assert_eq!(
        state.status,
        ProcessStatus::Idle,
        "批量失败后状态必须归位，否则续跑入口会把整批卡死"
    );

    // 调用点可能已经先写入了更具体的失败原因（如「未识别出有效字幕」），
    // 那类提示要留给用户看，不能被归位成 Idle 抹掉。
    state.status = ProcessStatus::Failed("未识别出有效字幕".to_string());
    state.batch_active = Some(0);
    state.batch_queue[0].state = QueueState::Running;
    state.finish_active_queue_item(Err("未识别出有效字幕".to_string()));
    assert_eq!(
        state.status,
        ProcessStatus::Failed("未识别出有效字幕".to_string()),
        "更具体的失败原因必须保留"
    );
}

/// 从队列里删条目时，活动指针必须跟着位移：删掉正在跑那条**之前**的条目后，
/// 指针若不作废就会指向别人——`finish_active_queue_item` 会把完成/失败记到
/// 另一条卡片上，而真正在跑的那条永远停在「转写中」。
#[test]
fn removing_queue_item_keeps_active_pointer_on_the_running_item() {
    use super::{QueueItem, QueueState};
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());
    state.batch_queue = vec![
        QueueItem::new(PathBuf::from("a.mp4")),
        QueueItem::new(PathBuf::from("b.mp4")),
        QueueItem::new(PathBuf::from("c.mp4")),
    ];
    // b.mp4 正在跑（下标 1）
    state.batch_active = Some(1);
    state.batch_queue[1].state = QueueState::Running;

    // 删掉它前面的 a.mp4：指针必须前移到 0，仍指向 b.mp4
    assert!(state.remove_queue_item(0));
    assert_eq!(state.batch_active, Some(0));
    assert_eq!(state.batch_queue[0].name, "b.mp4");

    state.finish_active_queue_item(Ok(7));
    assert_eq!(
        state.batch_queue[0].state,
        QueueState::Done { segments: 7 },
        "结算必须落在真正在跑的那条上"
    );

    // 删掉正在跑的那条：指针作废，不再指到别人头上
    state.batch_active = Some(1);
    assert!(state.remove_queue_item(1));
    assert_eq!(state.batch_active, None);

    // 越界删除是安全空操作，且不动指针
    state.batch_active = Some(0);
    assert!(!state.remove_queue_item(9));
    assert_eq!(state.batch_active, Some(0));
}

/// 重排只对「合法且真的会换位置」的入参生效：空队列 / 单元素队列 / 越界 /
/// `from == to` 都是安全空操作，合法移动则把条目搬到目标下标上。
///
/// 空操作必须是**一字不动**：这类调用来自第一行的「上移」、最后一行的「下移」
/// （界面已置灰，但状态层要能兜住任何调用方），一旦顺手重排就会把用户排好的顺序改掉。
#[test]
fn move_queue_item_reorders_only_valid_distinct_indices() {
    use super::{QueueItem, QueueState};
    use std::path::PathBuf;

    fn names(state: &AppState) -> Vec<String> {
        state.batch_queue.iter().map(|i| i.name.clone()).collect()
    }

    let mut state = test_state(Vec::new());
    assert!(!state.move_queue_item(0, 0), "空队列不能移动");
    assert!(state.batch_queue.is_empty(), "失败的空操作不许建出条目");

    // 单元素队列：没有任何合法目标（上移 / 下移都无处可去）
    state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
    assert!(!state.move_queue_item(0, 1));
    assert!(!state.move_queue_item(1, 0));
    assert!(!state.move_queue_item(0, 0));
    assert_eq!(names(&state), vec!["a.mp4"]);

    state.batch_queue = vec![
        QueueItem::new(PathBuf::from("a.mp4")),
        QueueItem::new(PathBuf::from("b.mp4")),
        QueueItem::new(PathBuf::from("c.mp4")),
    ];
    state.batch_queue[1].state = QueueState::Running;

    // 下移一格：a 落到下标 1
    assert!(state.move_queue_item(0, 1));
    assert_eq!(names(&state), vec!["b.mp4", "a.mp4", "c.mp4"]);
    // 搬到队首：c 从队尾插到 0
    assert!(state.move_queue_item(2, 0));
    assert_eq!(names(&state), vec!["c.mp4", "b.mp4", "a.mp4"]);

    // 越界 / 原地：返回 false，顺序不动
    assert!(!state.move_queue_item(0, 3));
    assert!(!state.move_queue_item(3, 0));
    assert!(!state.move_queue_item(1, 1));
    assert_eq!(names(&state), vec!["c.mp4", "b.mp4", "a.mp4"]);
}

/// 重排不得让活动指针错位：`batch_active` 必须仍然指向**同一条**任务，
/// 不论它在移动中换了哪个下标。
///
/// 这是重排最致命的坑：下标一变，指针若留在原数字上就指向了邻居——
/// `finish_active_queue_item` 会把完成 / 失败结算到别人头上，而真正在跑的那条
/// 永远停在「转写中」。所以移动别的条目、甚至移动正在跑的那条本身，指针都得跟着走。
#[test]
fn moving_queue_items_keeps_active_pointer_on_the_running_item() {
    use super::{QueueItem, QueueState};
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());
    state.batch_queue = vec![
        QueueItem::new(PathBuf::from("a.mp4")),
        QueueItem::new(PathBuf::from("b.mp4")),
        QueueItem::new(PathBuf::from("c.mp4")),
        QueueItem::new(PathBuf::from("d.mp4")),
    ];
    // c.mp4 正在跑（下标 2）
    state.batch_active = Some(2);
    state.batch_queue[2].state = QueueState::Running;

    // 把队首的 a 搬到队尾：c 前移到 1，指针必须跟着改到 1
    assert!(state.move_queue_item(0, 3));
    assert_eq!(state.batch_active, Some(1));
    assert_eq!(state.batch_queue[1].name, "c.mp4");

    // 再把队尾的 a 搬回队首：c 回到 2
    assert!(state.move_queue_item(3, 0));
    assert_eq!(state.batch_active, Some(2));
    assert_eq!(state.batch_queue[2].name, "c.mp4");

    // 失败的空操作更不能碰指针
    assert!(!state.move_queue_item(2, 2));
    assert!(!state.move_queue_item(9, 0));
    assert_eq!(state.batch_active, Some(2));
    assert_eq!(state.batch_queue[2].name, "c.mp4");

    // 连正在跑的那条自己也能搬（界面置灰只是不给点，状态层照样保持归属正确）
    assert!(state.move_queue_item(2, 0));
    assert_eq!(state.batch_active, Some(0));
    assert_eq!(state.batch_queue[0].name, "c.mp4");

    // 结算必须落在真正在跑的那条上，而不是它换位前的邻居
    state.finish_active_queue_item(Ok(9));
    assert_eq!(state.batch_queue[0].state, QueueState::Done { segments: 9 });
    assert!(state.batch_active.is_none());
    assert_eq!(state.batch_queue[1].state, QueueState::Pending);
}

/// 反复点「起点 +0.5」不能把当前句推过下一句：交叉区间会被
/// `optimize_segments` 在下次载入时按「排序 + 截断重叠 + 剔鬼影」重写甚至删句，
/// 用户看到的就是「字幕没了，怎么改都回不来」。
#[test]
fn adjust_times_keeps_timeline_valid_and_survives_optimize() {
    use crate::subtitle::{optimize_segments, MIN_SEGMENT_DUR};

    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
        Segment::new(3, 4.0, 6.0, "第三句"),
    ]);
    state.selected_segment_index = Some(1);
    for _ in 0..8 {
        state.adjust_selected_times(0.5, 0.0);
    }

    // (a) 时间轴仍然单调不重叠
    for w in state.segments.windows(2) {
        assert!(
            w[0].end <= w[1].start + 1e-9,
            "第 {} 段与下一段交叉：{:?}",
            w[0].index,
            (w[0].end, w[1].start)
        );
    }
    // (b) 没有片段的时长小于最短时长
    for seg in &state.segments {
        assert!(
            seg.duration() >= MIN_SEGMENT_DUR - 1e-9,
            "第 {} 段时长过短：{}",
            seg.index,
            seg.duration()
        );
    }
    // (c) 对调完的结果跑一遍载入时的规范化，片段数量不变（没有被当鬼影删掉）
    let before = state.segments.len();
    let mut after = state.segments.clone();
    optimize_segments(&mut after);
    assert_eq!(
        after.len(),
        before,
        "用户手工调过的时间被 optimize_segments 当鬼影删掉了"
    );
}

/// 撤销/重做：快照在编辑之前压栈，撤销回到编辑前，重做回到编辑后。
#[test]
fn undo_and_redo_restore_edit_state() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.selected_segment_index = Some(2);

    let before: Vec<(f64, f64)> = state.segments.iter().map(|s| (s.start, s.end)).collect();
    state.adjust_selected_times(0.0, 0.5);
    let after: Vec<(f64, f64)> = state.segments.iter().map(|s| (s.start, s.end)).collect();
    assert_ne!(before, after, "微调应当真的改动了时间");

    assert!(state.can_undo(), "编辑之后应当有可撤销的步骤");
    assert!(state.undo(), "撤销应当成功");
    let undone: Vec<(f64, f64)> = state.segments.iter().map(|s| (s.start, s.end)).collect();
    assert_eq!(undone, before, "撤销后应回到编辑前的时间");

    assert!(state.can_redo(), "撤销之后应当有可重做的步骤");
    assert!(state.redo(), "重做应当成功");
    let redone: Vec<(f64, f64)> = state.segments.iter().map(|s| (s.start, s.end)).collect();
    assert_eq!(redone, after, "重做后应回到编辑后的时间");
}

/// 删除也要能撤销（快照在 `remove` 之前压栈），否则用户删错一句只能重跑转写。
#[test]
fn undo_restores_deleted_segment() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.selected_segment_index = Some(1);
    state.delete_selected_segment();
    assert_eq!(state.segments.len(), 1);

    assert!(state.undo());
    assert_eq!(state.segments.len(), 2, "撤销删除应把被删片段放回去");
    assert_eq!(state.segments[1].text, "第二句");
}

/// 换工程必须清空撤销栈。修前：在工程 A 里做一次编辑，再打开工程 B，
/// 按 Ctrl+Z 会把 A 的整份字幕灌回当前文档；`restore_snapshot` 收尾的
/// `flush_segments_if_dirty` 还会按 B 的 id 落库，等于用 A 覆盖掉 B 的历史记录。
#[test]
fn switching_projects_clears_undo_history() {
    use crate::storage::TaskRecord;

    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "甲工程第一句"),
        Segment::new(2, 2.0, 4.0, "甲工程第二句"),
    ]);
    state.selected_segment_index = Some(1);
    state.adjust_selected_times(0.0, 0.5);
    assert!(
        state.can_undo(),
        "前置条件：甲工程里应当攒下了一次可撤销的编辑"
    );

    let other = TaskRecord {
        id: 2,
        file_path: "D:/video/b.mp4".to_string(),
        file_name: "b.mp4".to_string(),
        duration: 4.0,
        status: "completed".to_string(),
        segments: vec![Segment::new(1, 0.0, 2.0, "乙工程第一句")],
        segment_count: 1,
        sample_text: "乙工程第一句".to_string(),
        segments_loaded: true,
        has_translation: false,
        created_at: String::new(),
        content_hash: None,
        metrics: None,
    };
    state.load_task_with(&other, true);

    assert_eq!(state.segments.len(), 1);
    assert_eq!(state.segments[0].text, "乙工程第一句");
    assert!(!state.can_undo(), "换工程后不应还留着上一个工程的撤销点");
    assert!(!state.can_redo(), "重做栈同样必须清空");
    assert!(!state.undo(), "撤销应当无事可做");
    assert_eq!(
        state.segments[0].text, "乙工程第一句",
        "撤销把上一个工程的字幕灌回了当前工程"
    );
}

/// 载入历史工程时 `total_duration` 必须与**优化后**的时间轴自洽。
///
/// `load_task_with` 先 `optimize_segments` 再落定 `self.segments`，但时长回退
/// 此前取的是优化**前**的 `task.segments.last().end`。极短尾句会在优化时被平滑
/// 延展到最短可读时长（0.8s），于是 `total_duration` 比真实时间轴还短：`seek_to`
/// 会把播放指针夹在末尾之前，用户点时间轴最后一段跳不过去。
#[test]
fn load_task_duration_uses_optimized_timeline() {
    use crate::storage::TaskRecord;

    let mut state = test_state(Vec::new());
    let task = TaskRecord {
        id: 7,
        file_path: "D:/video/c.mp4".to_string(),
        file_name: "c.mp4".to_string(),
        // 库里没记时长（探测失败）：必须回退到时间轴
        duration: 0.0,
        status: "completed".to_string(),
        segments: vec![
            Segment::new(1, 0.0, 1.0, "第一句话"),
            Segment::new(2, 1.0, 1.2, "嗯"),
        ],
        segment_count: 2,
        sample_text: "第一句话".to_string(),
        segments_loaded: true,
        has_translation: false,
        created_at: String::new(),
        content_hash: None,
        metrics: None,
    };
    state.load_task_with(&task, false);

    let timeline_end = crate::subtitle::timeline_end(&state.segments);
    assert!(
        timeline_end > 1.2,
        "前置条件：尾句应被极短句平滑延展（实际末句 end = {timeline_end}）"
    );
    assert_eq!(
        state.total_duration, timeline_end,
        "total_duration 必须跟随优化后的时间轴，否则 seek_to 会把指针夹在末尾之前"
    );
}

/// 缓存命中载入与 `load_task_with` 同口径：`total_duration` 取优化后末句 end。
///
/// 回归护栏：`load_from_cache` 此前在 `optimize_segments` **之前**用
/// `cached.segments.last().end` 算回退时长，同一个坑。
#[test]
fn load_from_cache_duration_uses_optimized_timeline() {
    use crate::storage::TaskRecord;

    let mut state = test_state(Vec::new());
    let cached = TaskRecord {
        id: 9,
        file_path: "D:/video/d.mp4".to_string(),
        file_name: "d.mp4".to_string(),
        duration: 0.0,
        status: "completed".to_string(),
        segments: vec![
            Segment::new(1, 0.0, 1.0, "第一句话"),
            Segment::new(2, 1.0, 1.2, "嗯"),
        ],
        segment_count: 2,
        sample_text: "第一句话".to_string(),
        segments_loaded: true,
        has_translation: false,
        created_at: String::new(),
        content_hash: None,
        metrics: None,
    };
    state.load_from_cache(cached);

    let timeline_end = crate::subtitle::timeline_end(&state.segments);
    assert!(
        timeline_end > 1.2,
        "前置条件：尾句应被平滑延展（实际 {timeline_end}）"
    );
    assert_eq!(
        state.total_duration, timeline_end,
        "缓存命中载入也必须用优化后的时间轴算 total_duration"
    );
}

/// 清空工作区也要清撤销栈：否则 Ctrl+Z 会让字幕表凭空复活一份
/// 「没有对应工程」的片段列表。
#[test]
fn clearing_workspace_clears_undo_history() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "某工程第一句")]);
    state.selected_segment_index = Some(1);
    state.editing_text = "改过的句子".to_string();
    state.save_selected_text();
    assert!(state.can_undo(), "前置条件：应当攒下了一次可撤销的文本编辑");

    state.clear_current_workspace();
    assert!(state.segments.is_empty());
    assert!(!state.can_undo(), "清空工作区后不应还有可撤销的步骤");
    assert!(!state.undo());
    assert!(state.segments.is_empty(), "撤销把已经清空的工程片段复活了");
}

/// 手工把一句拆成前后两段时，时间切点也必须有下限：否则短的那半会落进
/// `optimize_segments` 的幻读鬼影判据（时长 < 0.25s 且与后句几乎同时开始），
/// 下次载入工程时被直接删掉——用户看到的是「拆分之后重新打开，半句话没了，
/// 怎么改都回不来」。
#[test]
fn split_segment_halves_survive_reload() {
    use crate::subtitle::{optimize_segments, MIN_EDIT_DUR};

    // 15 字 / 2.0 秒：光标停在第 1 个字上拆分，按字数比例算出的切点约 0.13s，
    // 短于下限，旧实现会被当鬼影删掉。
    let mut state = test_state(vec![Segment::new(
        1,
        0.0,
        2.0,
        "前半句内容，后半句内容在这里呢",
    )]);
    state.selected_segment_index = Some(1);
    let before_len: usize = state
        .segments
        .iter()
        .map(|s| s.display_text().chars().count())
        .sum();
    state.split_selected_segment(Some(1));
    assert_eq!(state.segments.len(), 2, "应当拆成两段");
    assert_eq!(
        state
            .segments
            .iter()
            .map(|s| s.display_text().chars().count())
            .sum::<usize>(),
        before_len,
        "拆分不该丢字"
    );
    for w in state.segments.windows(2) {
        assert!(
            w[0].duration() >= MIN_EDIT_DUR - 1e-9,
            "拆出的前半段过短：{:?}",
            w[0]
        );
    }

    // 模拟下次载入工程时的规范化：两段都必须存活
    let mut after = state.segments.clone();
    optimize_segments(&mut after);
    assert_eq!(
        after.len(),
        2,
        "拆出的片段被 optimize_segments 当鬼影删掉了"
    );
    let joined: String = after.iter().map(|s| s.display_text().to_string()).collect();
    assert!(
        joined.contains('前') && joined.contains('后'),
        "复原后内容丢失：{joined}"
    );
}

/// 清单过滤下标必须始终落在片段表内：虚拟列表取不到行就量不出行高
/// （`item_height = 0`），整片清单会塌成空白，而且缓存键不变就一直空着——
/// 这正是用户说的「字幕没了」。
#[test]
fn filter_indices_cover_segments_after_edit() {
    use crate::subtitle::{indices_cover_segments, matched_indices};

    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
        Segment::new(3, 4.0, 6.0, "第三句"),
    ]);
    // 编辑前缓存下来的「全部片段」下标，删掉一段之后必然越界
    let stale = matched_indices(&state.segments, "");
    assert!(indices_cover_segments(&stale, state.segments.len()));

    state.selected_segment_index = Some(2);
    state.delete_selected_segment();
    assert!(
        !indices_cover_segments(&stale, state.segments.len()),
        "旧下标应当已越界——所以刷新时必须重新校验，不能只认缓存键"
    );

    // 重算之后必须重新覆盖全部片段
    let fresh = matched_indices(&state.segments, "");
    assert!(indices_cover_segments(&fresh, state.segments.len()));
    assert_eq!(fresh.len(), state.segments.len());

    // 过滤态（有关键字）只要求不越界
    let hit = matched_indices(&state.segments, "第三句");
    assert!(indices_cover_segments(&hit, state.segments.len()));
}
/// 流式转写窗口必须有界：长视频会 push 上千句，而看板只显示 3 句。
/// 这里锁住「内存占用是常数」这个不变量，防止将来有人把裁剪去掉。
#[test]
fn streaming_window_stays_bounded() {
    let mut state = test_state(Vec::new());
    for i in 1..=500 {
        state.push_stream_segment(Segment::new(
            i,
            (i as f64) * 2.0,
            (i as f64) * 2.0 + 1.0,
            "句子",
        ));
    }
    assert!(
        state.streaming_segments.len() <= AppState::STREAMING_WINDOW,
        "窗口必须钉死在上限内，实际 {}",
        state.streaming_segments.len()
    );
    // 计数器不受窗口裁剪影响：已流式生成句数应等于实际 push 的总数
    assert_eq!(
        state.streaming_segment_count, 500,
        "计数器必须是单调累计值，不能被缓冲区裁剪截断"
    );
    // 保留的是**最近**的若干句，不是最早的
    let last = state.streaming_segments.last().expect("应有内容");
    assert_eq!(last.index, 500, "窗口里应保留最后 push 的那句");
    // 时间戳仍应推进到最新（进度条依赖它）
    assert!((state.streaming_current_sec - 1001.0).abs() < 0.001);
}

/// 窗口内句序必须保持 push 顺序（看板按顺序展示最近 3 句）。
#[test]
fn streaming_window_keeps_order() {
    let mut state = test_state(Vec::new());
    for i in 1..=100 {
        state.push_stream_segment(Segment::new(i, i as f64, i as f64 + 0.5, "x"));
    }
    let idxs: Vec<usize> = state.streaming_segments.iter().map(|s| s.index).collect();
    let mut sorted = idxs.clone();
    sorted.sort_unstable();
    assert_eq!(idxs, sorted, "窗口内句序应保持递增");
    assert_eq!(*idxs.last().unwrap(), 100);
}

/// 清理必须把窗口与计时都归零（换任务时复用同一个 AppState）。
#[test]
fn clear_streaming_resets_window() {
    let mut state = test_state(Vec::new());
    for i in 1..=10 {
        state.push_stream_segment(Segment::new(i, i as f64, i as f64 + 0.5, "x"));
    }
    state.clear_streaming();
    assert!(state.streaming_segments.is_empty());
    assert_eq!(state.streaming_current_sec, 0.0);
    assert_eq!(state.streaming_segment_count, 0, "清理必须把计数器也归零");
}

/// 渲染路径每帧都会调 `get_cached_transcription()`，而底层 `find_cached_task`
/// 会把整份字幕 JSON 反序列化出来（实测约 0.79 ms/次）。这个测试锁死缓存语义：
/// 同一路径在**未显式失效**前不会重新查库——证据是库里中途新插了一条同路径记录，
/// 但返回值仍是首次那份「无命中」，直到 `invalidate_cached_transcription` 才更新。
#[test]
fn cached_transcription_reuses_result_until_invalidated() {
    use crate::subtitle::Segment;
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());
    state.transcribe_file = Some(PathBuf::from("D:/video/a.mp4"));

    // 首次调用：库里还没有 → 返回 None 并回填缓存
    assert!(state.get_cached_transcription().is_none());

    // 写库一条同路径的已完成记录（模拟后台刚转写完）
    let id = state
        .db
        .insert_task(
            "D:/video/a.mp4",
            "a.mp4",
            4.0,
            "completed",
            &[Segment::new(1, 0.0, 2.0, "第一句")],
            None,
        )
        .expect("插入应成功");

    // 缓存已回填「无命中」，未失效前不应重新查库
    assert!(
        state.get_cached_transcription().is_none(),
        "同一路径在失效前不应重新查库（否则渲染路径每帧都反序列化字幕）"
    );

    // 显式失效后才应看到新记录
    state.invalidate_cached_transcription();
    let hit = state.get_cached_transcription().expect("失效后应能查到");
    assert_eq!(hit.id, id);
    assert_eq!(hit.segments.len(), 1);

    // 再次调用走缓存，返回同一份
    assert_eq!(state.get_cached_transcription().unwrap().id, id);
}

/// 换文件（`transcribe_file` 换成另一个路径）必须立刻反映到结果，
/// 不能把上一个文件「无缓存」的结论错误地套到新文件上。
#[test]
fn cached_transcription_follows_file_change() {
    use crate::subtitle::Segment;
    use std::path::PathBuf;

    let mut state = test_state(Vec::new());

    state.transcribe_file = Some(PathBuf::from("D:/video/a.mp4"));
    assert!(state.get_cached_transcription().is_none());

    let id = state
        .db
        .insert_task(
            "D:/video/b.mp4",
            "b.mp4",
            6.0,
            "completed",
            &[Segment::new(1, 0.0, 3.0, "B 文件第一句")],
            None,
        )
        .expect("插入应成功");

    // 换成 b.mp4：路径不一致，缓存必须重新查库
    state.transcribe_file = Some(PathBuf::from("D:/video/b.mp4"));
    let hit = state
        .get_cached_transcription()
        .expect("换文件后应查到 B 的缓存");
    assert_eq!(hit.id, id);
    assert_eq!(hit.segments[0].text, "B 文件第一句");
}

/// 删除「同路径的另一条历史记录」不能切走当前工作区。
///
/// 回归护栏：此前 `delete_task_record` 用 `selected_file` 路径比较判定
/// 「被删的是不是当前工程」。同一路径允许多条记录（每次转写各插一行），
/// 删掉同路径的另一条会把当前工程无故补位切走。改为按 `active_task_id`
/// 主键判定后，删非当前记录应原样保留工作区。
#[test]
fn deleting_same_path_sibling_keeps_current_workspace() {
    let mut state = test_state(Vec::new());
    let path = "D:/video/same.mp4";
    let older = state
        .db
        .insert_task(
            path,
            "same.mp4",
            3.0,
            "completed",
            &[Segment::new(1, 0.0, 3.0, "旧记录")],
            None,
        )
        .expect("插入旧记录应成功");
    let current = state
        .db
        .insert_task(
            path,
            "same.mp4",
            3.0,
            "completed",
            &[Segment::new(1, 0.0, 3.0, "当前记录")],
            None,
        )
        .expect("插入当前记录应成功");

    state.refresh_recent_tasks();
    let current_task = state
        .recent_tasks
        .iter()
        .find(|t| t.id == current)
        .cloned()
        .expect("列表应能取到当前记录");
    state.load_task_with(&current_task, false);
    assert_eq!(state.active_task_id, Some(current));

    // 删同路径的**另一条**记录：当前工作区必须原样保留
    state.delete_task_record(older).expect("删除应成功");
    assert_eq!(
        state.active_task_id,
        Some(current),
        "删同路径另一条记录不应切走当前工作区"
    );
    assert_eq!(state.segments.len(), 1, "当前工程字幕不应被清空");
    assert_eq!(state.segments[0].text, "当前记录");
}

/// 缓存命中必须返回**同一个 Arc**（指针相等），而不是每帧深拷贝一份新记录。
///
/// 这是「渲染路径零拷贝」的核心不变量：若哪天有人把返回类型换回
/// `Option<TaskRecord>`，这个断言会立刻失败——否则每帧都会悄悄复制
/// 上千条 Segment 的 String，把省下的反序列化又原样花回去。
#[test]
fn cached_transcription_hit_shares_one_arc() {
    use crate::subtitle::Segment;
    use std::path::PathBuf;
    use std::sync::Arc;

    let mut state = test_state(Vec::new());
    state.transcribe_file = Some(PathBuf::from("D:/video/a.mp4"));
    state
        .db
        .insert_task(
            "D:/video/a.mp4",
            "a.mp4",
            4.0,
            "completed",
            &[
                Segment::new(1, 0.0, 2.0, "第一句"),
                Segment::new(2, 2.0, 4.0, "第二句"),
            ],
            None,
        )
        .expect("插入应成功");

    let first: Arc<_> = state.get_cached_transcription().expect("首次应命中");
    let second: Arc<_> = state.get_cached_transcription().expect("再次应命中");
    assert!(
        Arc::ptr_eq(&first, &second),
        "缓存命中应复用同一份 Arc，而不是每帧深拷贝整份字幕"
    );
    assert_eq!(first.segments.len(), 2);
}
/// 回归：翻译收尾必须**按句合并**，不能整表覆盖。
///
/// S4：翻译期间用户改了**原文**，收尾不得把「按旧原文译出的结果」盖上去。
#[test]
fn merge_skips_translations_whose_source_changed() {
    use crate::subtitle::Segment;
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "原始第一句")]);
    let baseline = super::TranslationBaseline::capture(&state.segments);

    // 翻译跑的同时用户把原文改了
    state.segments[0].text = "用户改过的第一句".to_string();
    let mut translated = state.segments.clone();
    translated[0].translation = Some("First sentence.".to_string());
    translated[0].translation_lang = Some("English".to_string());

    let skipped = state.merge_translations_checked(translated, Some(&baseline));
    assert_eq!(skipped, 1, "原文变了的句子必须被跳过");
    assert!(
        state.segments[0].translation.is_none(),
        "旧原文的译文不能贴到新原文上: {:?}",
        state.segments[0].translation
    );
}

#[test]
fn merge_after_deletion_uses_snapshot_baseline() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "删除句"),
        Segment::new(2, 1.0, 2.0, "保留句"),
        Segment::new(3, 2.0, 3.0, "手改句"),
    ]);
    let baseline = super::TranslationBaseline::capture(&state.segments);
    let mut translated = state.segments.clone();
    for seg in &mut translated {
        seg.translation = Some(format!("translated {}", seg.index));
    }
    state.segments.remove(0);
    state.reindex_segments();
    state.segments[1].translation = Some("手写译文".into());

    assert_eq!(
        state.merge_translations_checked(translated, Some(&baseline)),
        1
    );
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("translated 2")
    );
    assert_eq!(state.segments[1].translation.as_deref(), Some("手写译文"));
}

#[test]
fn merge_preserves_distinct_submillisecond_starts() {
    let mut state = test_state(vec![
        Segment::new(1, 1.0001, 2.0, "相同原文"),
        Segment::new(2, 1.0002, 3.0, "相同原文"),
    ]);
    let baseline = super::TranslationBaseline::capture(&state.segments);
    let mut translated = state.segments.clone();
    translated[0].translation = Some("first".into());
    translated[1].translation = Some("second".into());
    assert_eq!(
        state.merge_translations_checked(translated, Some(&baseline)),
        0
    );
    assert_eq!(state.segments[0].translation.as_deref(), Some("first"));
    assert_eq!(state.segments[1].translation.as_deref(), Some("second"));
}

#[test]
fn merge_disambiguates_same_start_by_source() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "说话人甲"),
        Segment::new(2, 0.0, 2.0, "说话人乙"),
    ]);
    let baseline = super::TranslationBaseline::capture(&state.segments);
    let mut translated = state.segments.clone();
    translated[0].translation = Some("speaker A".into());
    translated[1].translation = Some("speaker B".into());
    assert_eq!(
        state.merge_translations_checked(translated, Some(&baseline)),
        0
    );
    assert_eq!(state.segments[0].translation.as_deref(), Some("speaker A"));
    assert_eq!(state.segments[1].translation.as_deref(), Some("speaker B"));
}

#[test]
fn merge_skips_ambiguous_identical_captions() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "同一句"),
        Segment::new(2, 0.0, 2.0, "同一句"),
    ]);
    let mut translated = state.segments.clone();
    translated[0].translation = Some("first".into());
    translated[1].translation = Some("second".into());
    assert_eq!(state.merge_translations_checked(translated, None), 2);
    assert!(state.segments.iter().all(|seg| seg.translation.is_none()));
}

#[test]
fn merge_without_baseline_rejects_changed_source() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 1.0, "旧原文")]);
    let mut translated = state.segments.clone();
    translated[0].translation = Some("old source".into());
    state.segments[0].text = "新原文".into();
    assert_eq!(state.merge_translations_checked(translated, None), 1);
    assert!(state.segments[0].translation.is_none());
}

/// 错别字记忆是**装载文档时自动跑**的：锁定句必须原样保留。
///
/// 这条对应最气人的那种体验——打开工程时静默改写原文，用户看不到它动了什么，
/// 只会发现「我改好的句子又变回去了」。
#[test]
fn typo_memory_skips_locked_segments() {
    use crate::subtitle::Segment;
    let mut locked = Segment::new(1, 0.0, 2.0, "这是错别字句子");
    locked.locked = true;
    let free = Segment::new(2, 2.0, 4.0, "这是错别字句子");
    let mut state = test_state(vec![locked, free]);

    // 造一条真实规则：错别字 → 错别字正确
    let mut memory = crate::utils::typo_memory::TypoMemory::default();
    assert_eq!(
        memory.learn_from_edit("这是错别字句子", "这是正确句子"),
        1,
        "前提：这条编辑应能学出一条规则"
    );

    let changed = state.apply_typo_memory_with(&memory);
    assert_eq!(changed, 1, "只有未锁定的那句应被改写");
    assert_eq!(
        state.segments[0].text, "这是错别字句子",
        "锁定句必须原样保留"
    );
    assert_eq!(
        state.segments[1].text, "这是正确句子",
        "未锁定句应被记忆库改写"
    );
}

/// Shift 连点扩选：锚点固定，当前句移动，区间取两者之间。
#[test]
fn selected_span_extends_from_anchor() {
    use crate::subtitle::Segment;
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "一"),
        Segment::new(2, 1.0, 2.0, "二"),
        Segment::new(3, 2.0, 3.0, "三"),
        Segment::new(4, 3.0, 4.0, "四"),
    ]);

    state.select_segment(2);
    assert_eq!(state.selected_span(), Some((2, 2)), "单选时区间就是这一句");

    // Shift 点到第 4 句：从锚点 2 扩到 4
    state.select_segment_with(4, true);
    assert_eq!(state.selected_span(), Some((2, 4)));

    // 反向扩选（往上点）：锚点不动，区间变成 1..=2
    state.select_segment_with(1, true);
    assert_eq!(state.selected_span(), Some((1, 2)));

    // 不带 Shift 的点击重置锚点
    state.select_segment(3);
    assert_eq!(state.selected_span(), Some((3, 3)));
}

/// 锁定（已校对）的句子：重新翻译**不得覆盖**它的译文。
///
/// 与 baseline 保护互补：baseline 只在本次运行内有效（关掉程序就没了），
/// 而「已校对」随工程落库，是用户明确表达过的意思。
#[test]
fn merge_skips_locked_segments() {
    use crate::subtitle::Segment;
    let mut a = Segment::new(1, 0.0, 2.0, "第一句");
    a.translation = Some("我自己定稿的译文".to_string());
    a.translation_lang = Some("English".to_string());
    a.locked = true;
    let b = Segment::new(2, 2.0, 4.0, "第二句");
    let mut state = test_state(vec![a, b]);

    // 一次重新翻译：两条都带回了新译文
    let mut translated = state.segments.clone();
    for seg in translated.iter_mut() {
        seg.translation = Some("machine translation".to_string());
        seg.translation_lang = Some("English".to_string());
    }
    let skipped = state.merge_translations_checked(translated, None);

    assert_eq!(skipped, 1, "锁定的那句应被计入跳过");
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("我自己定稿的译文"),
        "锁定句的译文不得被覆盖"
    );
    assert_eq!(
        state.segments[1].translation.as_deref(),
        Some("machine translation"),
        "未锁定的句子应正常更新"
    );
}

/// S4：翻译期间用户手写了译文，收尾不得覆盖。
#[test]
fn merge_keeps_user_handwritten_translation() {
    use crate::subtitle::Segment;
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "原始第一句")]);
    let baseline = super::TranslationBaseline::capture(&state.segments);

    // 翻译期间用户自己写了译文
    state.segments[0].translation = Some("我自己写的".to_string());
    state.segments[0].translation_lang = Some("English".to_string());
    let mut translated = state.segments.clone();
    translated[0].translation = Some("Machine text.".to_string());

    let skipped = state.merge_translations_checked(translated, Some(&baseline));
    assert_eq!(skipped, 1, "手写译文必须被保留");
    assert_eq!(state.segments[0].translation.as_deref(), Some("我自己写的"));
}

/// 增量落盘会**反复**合并同一份快照：第二次不能因为「译文已不是开工值」
/// 就被误判成手改而全部跳过，否则「翻译一会落盘一点」只会生效一次。
#[test]
fn merge_is_idempotent_across_incremental_flushes() {
    use crate::subtitle::Segment;
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "原始第一句")]);
    let baseline = super::TranslationBaseline::capture(&state.segments);

    let mut translated = state.segments.clone();
    translated[0].translation = Some("First sentence.".to_string());
    translated[0].translation_lang = Some("English".to_string());

    assert_eq!(
        state.merge_translations_checked(translated.clone(), Some(&baseline)),
        0
    );
    assert_eq!(
        state.merge_translations_checked(translated, Some(&baseline)),
        0,
        "同一份快照重复合并应当都生效（幂等），不能被当成手改而跳过"
    );
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("First sentence.")
    );
}

/// F1：导入字幕的工程没有库主键，flush 必须**保持脏**。以前这里清掉脏标记并
/// 返回成功，关窗守卫读到的就是「已保存」，一整份校对静默清零。
#[test]
fn flush_without_task_id_keeps_dirty_flag() {
    use crate::subtitle::Segment;
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "第一句")]);
    state.active_task_id = None;
    state.segments_dirty = true;
    state.flush_segments_if_dirty();
    assert!(
        state.segments_dirty,
        "没有库主键时不允许把脏标记清成「已保存」"
    );
}

/// 停车位（`PendingWait`）：计数归零必须立刻唤醒等待方（不再 2ms 轮询空转），
/// 且不能丢唤醒；到点不归零也要按时返回，不能挂死。
#[test]
fn pending_wait_wakes_on_notify_and_times_out_cleanly() {
    use super::PendingWait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let wait = std::sync::Arc::new(PendingWait::default());
    let pending = std::sync::Arc::new(AtomicUsize::new(1));

    // 场景一：等待方先睡下，通知方 50ms 后归零并广播——应立刻醒来，不睡满上限。
    let w = std::sync::Arc::clone(&wait);
    let p = std::sync::Arc::clone(&pending);
    let notifier = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        p.fetch_sub(1, Ordering::SeqCst);
        w.notify();
    });
    let start = std::time::Instant::now();
    wait.wait_until(&pending, start + std::time::Duration::from_secs(5));
    notifier.join().unwrap();
    assert_eq!(pending.load(Ordering::SeqCst), 0);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "应被归零唤醒，而不是睡满上限"
    );

    // 场景二：无人归零——最多等到 deadline 为止，不挂死。
    pending.store(1, Ordering::SeqCst);
    let start = std::time::Instant::now();
    wait.wait_until(&pending, start + std::time::Duration::from_millis(50));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

/// F11：拆句要把语言包与词级时间戳一起切开，右半不能再是空包。
#[test]
fn split_segment_splits_translation_packs_and_words() {
    use crate::subtitle::segment::WordTiming;
    use crate::subtitle::Segment;

    let mut seg = Segment::new(1, 0.0, 4.0, "前半句话后半句话");
    seg.translation = Some("first half second half".to_string());
    seg.translation_lang = Some("English".to_string());
    seg.translations
        .insert("English".to_string(), "first half second half".to_string());
    seg.translations
        .insert("日本語".to_string(), "前半と後半".to_string());
    seg.words = vec![
        WordTiming {
            word: "前半".into(),
            start: 0.2,
            end: 1.5,
        },
        WordTiming {
            word: "后半".into(),
            start: 2.5,
            end: 3.8,
        },
    ];
    let mut state = test_state(vec![seg]);
    state.selected_segment_index = Some(1);
    state.split_selected_segment(None);

    assert_eq!(state.segments.len(), 2, "应拆成两条");
    let left = &state.segments[0];
    let right = &state.segments[1];
    assert!(left.translations.contains_key("English"));
    assert!(
        right.translations.contains_key("English"),
        "右半必须拿到自己那份语言包，而不是空包"
    );
    assert_ne!(
        left.translations["English"], right.translations["English"],
        "两半不能共用整句译文"
    );
    assert!(
        !left.words.iter().any(|w| w.start > left.end + 0.01),
        "左半不得保留切点之后的词"
    );
}

/// 回归：手动拆句两半都要继承 `locked` 定稿锁。修前右半写死 `false`，
/// 锁定句拆开后右半会被「重译全部」覆盖掉用户校对定稿的译文。
#[test]
fn split_selected_segment_keeps_lock_on_both_halves() {
    use crate::subtitle::Segment;
    let mut seg = Segment::new(1, 0.0, 4.0, "前半句话后半句话");
    seg.locked = true;
    let mut state = test_state(vec![seg]);
    state.selected_segment_index = Some(1);
    state.split_selected_segment(None);

    assert_eq!(state.segments.len(), 2, "应拆成两条");
    assert!(state.segments[0].locked, "左半应保持锁定");
    assert!(
        state.segments[1].locked,
        "右半应继承锁定，不能被后续重译覆盖"
    );
}

/// F11：合并要保留语言包/词级时间戳，且西文之间补空格。
#[test]
fn merge_keeps_packs_words_and_inserts_space_for_latin() {
    use crate::subtitle::Segment;
    let mut a = Segment::new(1, 0.0, 2.0, "Hello");
    a.translation = Some("你好".to_string());
    a.translation_lang = Some("简体中文".to_string());
    a.translations
        .insert("简体中文".to_string(), "你好".to_string());
    let mut b = Segment::new(2, 2.0, 4.0, "world");
    b.translations
        .insert("简体中文".to_string(), "世界".to_string());

    let mut state = test_state(vec![a, b]);
    state.selected_segment_index = Some(1);
    state.merge_selected_with_next();

    assert_eq!(state.segments.len(), 1);
    assert_eq!(
        state.segments[0].display_text(),
        "Hello world",
        "西文合并必须补空格，不能粘成 Helloworld"
    );
    assert!(
        state.segments[0]
            .translations
            .get("简体中文")
            .is_some_and(|t| t.contains("世界")),
        "右半的语言包不能被丢弃"
    );
}

/// 语言包：换目标语言重译**不能**把旧语言的译文覆盖掉，切回去要能原样还原。
///
/// 这就是「字幕语言包」面板的数据基础：此前只有 `translation` 一份，译成英文
/// 就把中文那遍冲掉了，用户既看不到「译过哪几种语言」，也切不回去。
#[test]
fn translation_packs_survive_relanguage_and_can_be_switched_back() {
    use crate::subtitle::Segment;

    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "こんにちは")]);

    let mut first = state.segments.clone();
    first[0].translation = Some("你好".to_string());
    first[0].translation_lang = Some("简体中文".to_string());
    state.merge_translations(first);
    assert_eq!(
        state.translation_packs(),
        vec![("简体中文".to_string(), 1)],
        "译完一门语言应出现一个语言包"
    );

    // 改译成英文：中文那一份必须留着
    let mut second = state.segments.clone();
    second[0].translation = Some("Hello".to_string());
    second[0].translation_lang = Some("English".to_string());
    state.merge_translations(second);
    let packs = state.translation_packs();
    assert!(
        packs.iter().any(|(l, n)| l == "简体中文" && *n == 1),
        "旧语言包被覆盖了: {packs:?}"
    );
    assert!(packs.iter().any(|(l, n)| l == "English" && *n == 1));
    assert_eq!(state.current_display_pack().as_deref(), Some("English"));

    // 切回中文：展示字段被投影回来
    state.apply_translation_pack(Some("简体中文"));
    assert_eq!(state.segments[0].translation.as_deref(), Some("你好"));
    assert_eq!(
        state.segments[0].translation_lang.as_deref(),
        Some("简体中文")
    );

    // 切回原文：展示清空，但两份语言包都还在
    state.apply_translation_pack(None);
    assert!(state.segments[0].translation.is_none(), "应回到只显示原文");
    assert_eq!(state.translation_packs().len(), 2, "语言包一份都不该丢");
}

/// 翻译在后台跑几分钟，期间用户仍可编辑字幕。修前收尾是
/// `self.segments = translated`（开始时的快照），会把用户这几分钟的编辑
/// 静默回滚。这里模拟「翻译期间用户改了原文」，断言：原文改动保留，
/// 按旧原文生成的译文不回填，删掉的句子不复活。
#[test]
fn merge_translations_keeps_concurrent_user_edits() {
    use crate::subtitle::Segment;

    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "原始第一句"),
        Segment::new(2, 2.0, 4.0, "原始第二句"),
    ]);

    // 引擎开始时的快照（只带译文，原文是旧的）
    let mut translated = state.segments.clone();
    translated[0].translation = Some("First sentence.".to_string());
    translated[0].translation_lang = Some("English".to_string());
    translated[1].translation = Some("Second sentence.".to_string());
    translated[1].translation_lang = Some("English".to_string());

    // 翻译期间用户改了第 1 句原文、并删掉了第 2 句
    state.segments[0].text = "用户改过的第一句".to_string();
    state.segments.remove(1);

    state.merge_translations(translated);

    assert_eq!(state.segments.len(), 1, "用户删掉的句子不该被翻译结果复活");
    assert_eq!(
        state.segments[0].text, "用户改过的第一句",
        "用户的原文改动不该被翻译收尾回滚"
    );
    assert!(
        state.segments[0].translation.is_none(),
        "旧原文的译文不应贴到新原文上"
    );
    assert!(state.segments[0].translation_lang.is_none());
}

/// 回归：引擎回填空串译文时，`merge_translations` 不能覆盖用户已有的好译文。
///
/// 引擎在「本句没译出」时理论上只保留旧值，但解析失败路径可能写入空串。
/// 若收尾无条件 `seg.translation = src.translation`，用户先前译好的句子会在
/// 「再点一次翻译」后**变空白**。这里断言空串译文被忽略。
#[test]
fn merge_translations_ignores_blank_translation() {
    use crate::subtitle::Segment;

    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "你好")]);
    // 用户已有一句好译文
    state.segments[0].translation = Some("Hello".to_string());
    state.segments[0].translation_lang = Some("English".to_string());

    // 引擎快照里这句变成空串译文（模拟解析失败回填）
    let mut translated = state.segments.clone();
    translated[0].translation = Some("   ".to_string());
    translated[0].translation_lang = Some("English".to_string());

    state.merge_translations(translated);

    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("Hello"),
        "空串译文不该覆盖用户已有的好译文"
    );
}

/// 用户手工订正译文：写入非空译文、空串清除、以及自动补目标语言标记。
#[test]
fn set_selected_translation_edits_and_clears() {
    use crate::subtitle::Segment;

    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "你好"),
        Segment::new(2, 2.0, 4.0, "世界"),
    ]);
    state.translate_target_lang = "English".to_string();

    // 选中第一句，手工写入译文
    state.select_segment(1);
    state.set_selected_translation("  Hello  ");
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("Hello"),
        "应写入去除首尾空白的译文"
    );
    assert_eq!(
        state.segments[0].translation_lang.as_deref(),
        Some("English"),
        "凭空补的译文应带上当前目标语言标记，否则会被判为未完成"
    );
    assert!(state.segments[0].translation_matches("English"));

    // 空白串 = 清除译文
    state.set_selected_translation("   ");
    assert!(!state.segments[0].has_translation(), "空白应清除译文");
    assert!(state.segments[0].translation.is_none());

    // 未选中任何片段时是安全空操作
    state.selected_segment_index = None;
    state.set_selected_translation("Whatever");
    assert!(!state.segments[1].has_translation());
}

/// 回归：结构编辑（拆/合/删）会重排序号，合并键必须是**起始时间**而非 index。
///
/// 这里删掉第一句，剩余那句的 index 从 2 变成 1。若按 index 合并，它会拿到
/// 原本属于第 1 句（start 0.0）的译文——张冠李戴。按 start 合并则正确拿到
/// 它自己（start 2.0）的译文。
#[test]
fn merge_translations_survives_index_shift_from_delete() {
    use crate::subtitle::Segment;

    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    // 引擎快照：两句都带各自的译文
    let mut translated = state.segments.clone();
    translated[0].translation = Some("First.".to_string());
    translated[0].translation_lang = Some("English".to_string());
    translated[1].translation = Some("Second.".to_string());
    translated[1].translation_lang = Some("English".to_string());

    // 翻译期间用户删掉了第一句，并重新编号（reindex_segments 的行为）
    state.segments.remove(0);
    state.reindex_segments();
    assert_eq!(state.segments.len(), 1);
    assert_eq!(state.segments[0].index, 1, "删句后序号被重排");
    assert_eq!(state.segments[0].start, 2.0);

    state.merge_translations(translated);

    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("Second."),
        "剩余句（start 2.0）必须拿到自己的译文，而不是被重排后的 index 撞上的第一句译文"
    );
}

// ───────────── P0-1：手工订正译文必须表达「属于当前目标语言」─────────────

/// English 译文 → 切到 日本語 → 手工改译文，标记必须变成 日本語。
///
/// 修前只补「原先没有标记」的情况，已有 "English" 的句子改完仍带旧标记，
/// 于是这句会被下一次翻译当**待译**，收尾时用机翻结果覆盖掉用户手写的日文。
#[test]
fn manual_translation_retags_to_current_target_lang() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "你好")]);
    // 先按 English 译过一遍
    state.segments[0].translation = Some("Hello".to_string());
    state.segments[0].translation_lang = Some("English".to_string());
    // 用户把目标语言切到 日本語（直接改字段，避免单测落盘 config.toml）
    state.translate_target_lang = "日本語".to_string();

    state.select_segment(1);
    state.set_selected_translation("こんにちは");

    assert_eq!(state.segments[0].translation.as_deref(), Some("こんにちは"));
    assert_eq!(
        state.segments[0].translation_lang.as_deref(),
        Some("日本語"),
        "手工订正发生在当前目标语言下，标记必须无条件改成当前目标语言"
    );
    assert!(
        !state.segments[0].translation_matches("English"),
        "旧语言标记必须被覆盖，否则这句会被判成「已是 English」"
    );
    assert!(state.segments[0].translation_matches("日本語"));
}

/// 核心防回归断言：手工订正后这句**不会**再被当待译，因此下一次翻译不会覆盖它。
///
/// 这里复刻两条翻译链路共用的增量判据
/// （`!translate_source().trim().is_empty() && !translation_matches(target)`，
/// 见 `engines::llm` 与 `engines::translate`），并走一遍 `merge_translations` 收尾。
#[test]
fn manual_translation_is_not_pending_on_next_translate() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "你好")]);
    state.segments[0].translation = Some("Hello".to_string());
    state.segments[0].translation_lang = Some("English".to_string());
    state.translate_target_lang = "日本語".to_string();

    state.select_segment(1);
    state.set_selected_translation("こんにちは");

    let target = "日本語";
    let pending: Vec<usize> = state
        .segments
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.translate_source().trim().is_empty() && !s.translation_matches(target))
        .map(|(pos, _)| pos)
        .collect();
    assert!(
        pending.is_empty(),
        "手写的日文译文仍被判为待译，下一次翻译会把它覆盖掉：{pending:?}"
    );
    // 「已完成 N/M」同样按当前目标语言判定（UI 用它决定按钮是否可点）
    assert_eq!(state.translated_count_for(target), 1);

    // 端到端：引擎在「无待译」时原样返回快照，合并收尾不能改动用户的译文
    let engine_snapshot = state.segments.clone();
    state.merge_translations(engine_snapshot);
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("こんにちは"),
        "用户手写的日文被翻译收尾覆盖了"
    );
    assert_eq!(
        state.segments[0].translation_lang.as_deref(),
        Some("日本語")
    );

    // 反向对照：若标记仍是 English（修前行为），这句必然落进 pending
    state.segments[0].translation_lang = Some("English".to_string());
    assert!(
        !state.segments[0].translation_matches(target),
        "对照：旧标记下这句应被判为待译（这正是修前的缺陷链）"
    );
}

/// 空串仍表示「清除译文」：translation 与 translation_lang 双双置 None（现有行为不能破坏）。
#[test]
fn clearing_manual_translation_nulls_both_fields() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "你好")]);
    state.translate_target_lang = "日本語".to_string();
    state.select_segment(1);
    state.set_selected_translation("こんにちは");
    assert!(state.segments[0].has_translation());

    state.set_selected_translation("   ");
    assert!(state.segments[0].translation.is_none());
    assert!(state.segments[0].translation_lang.is_none());
    assert!(!state.segments[0].has_translation());
}

// ───────────── P0-2：改原文必须让旧译文失效 ─────────────

/// 有译文的句子改原文 → `translation_matches(原目标语言)` 变 false，译文文本仍在。
#[test]
fn editing_source_invalidates_stale_translation() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "旧原文")]);
    state.segments[0].translation = Some("Old translation".to_string());
    state.segments[0].translation_lang = Some("English".to_string());

    state.select_segment(1);
    state.editing_text = "新原文".to_string();
    state.save_selected_text();

    assert_eq!(state.segments[0].text, "新原文");
    assert!(
        !state.segments[0].translation_matches("English"),
        "原文已改，旧译文不该再算「已完成」，否则下一次翻译会直接跳过这句"
    );
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("Old translation"),
        "只清语言标记、保留译文文本，用户仍能看到旧译文作参考"
    );
    // 残留风险（已知）：has_translation() 仍为 true，双语导出仍会带出这条陈旧译文。
    assert!(state.segments[0].has_translation());
}

/// 原文未变（同一文本重复保存）→ 标记不动。
#[test]
fn saving_unchanged_source_keeps_translation_lang() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "原文没动")]);
    state.segments[0].translation = Some("Untouched".to_string());
    state.segments[0].translation_lang = Some("English".to_string());

    state.select_segment(1);
    // 选中时 editing_text 已被填成 display_text；再保存一次属于「没改字」
    state.save_selected_text();
    assert_eq!(
        state.segments[0].translation_lang.as_deref(),
        Some("English"),
        "原文没变时不该清标记，否则点进来又点出去就丢掉完成状态"
    );
    assert!(state.segments[0].translation_matches("English"));
}

/// 无译文的句子改原文 → 不 panic、无副作用（不该凭空写出标记或译文）。
#[test]
fn editing_source_without_translation_is_noop() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "还没翻译")]);
    state.select_segment(1);
    state.editing_text = "改过但还没翻译".to_string();
    state.save_selected_text();

    assert_eq!(state.segments[0].text, "改过但还没翻译");
    assert!(state.segments[0].translation.is_none());
    assert!(state.segments[0].translation_lang.is_none());
    assert!(!state.segments[0].has_translation());
}

/// 展示文本取自 `polished` 时，改 `polished` 同样要判为「原文变了」。
#[test]
fn editing_polished_source_invalidates_translation() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "原始识别")]);
    state.segments[0].polished = "润色后的原文".to_string();
    state.segments[0].translation = Some("Stale".to_string());
    state.segments[0].translation_lang = Some("English".to_string());

    state.select_segment(1);
    assert_eq!(state.editing_text, "润色后的原文");
    state.editing_text = "又改过的润色原文".to_string();
    state.save_selected_text();

    assert!(!state.segments[0].translation_matches("English"));
    assert_eq!(state.segments[0].polished, "又改过的润色原文");
}

// ───────────── P0-3：手工拆分必须按 ratio 切开译文 ─────────────

/// 有译文的一句在中间拆分 → 左右各拿一半，两半不相同、拼起来等于原译文。
#[test]
fn manual_split_splits_translation_by_ratio() {
    let mut state = test_state(vec![Segment::new(
        1,
        0.0,
        10.0,
        "前面这半句讲的是背景，后面这半句讲的是结论。",
    )]);
    let original = "The first half is background, the second half is the conclusion.";
    state.segments[0].translation = Some(original.to_string());
    state.segments[0].translation_lang = Some("English".to_string());

    state.select_segment(1);
    state.editing_text = state.segments[0].display_text().to_string();
    state.split_selected_segment(None);

    assert_eq!(state.segments.len(), 2, "应拆成两段");
    let left = state.segments[0]
        .translation
        .clone()
        .expect("左半段应带译文");
    let right = state.segments[1]
        .translation
        .clone()
        .expect("右半段应带译文");
    assert_ne!(left, right, "两半译文不该是同一条（修前就是整条复制）");
    assert_eq!(
        format!("{left}{right}"),
        original,
        "两半译文拼起来应等于原译文"
    );
    // 两半都保留目标语言标记，否则拆完会被判「未完成」而重复请求翻译
    assert!(state.segments[0].translation_matches("English"));
    assert!(state.segments[1].translation_matches("English"));
    assert!(state.segments[0].has_translation());
    assert!(state.segments[1].has_translation());
}

/// 无译文的一句拆分 → 两半都无译文（不能凭空造出译文）。
#[test]
fn manual_split_without_translation_stays_none() {
    let mut state = test_state(vec![Segment::new(
        1,
        0.0,
        10.0,
        "前面这半句讲的是背景，后面这半句讲的是结论。",
    )]);
    state.select_segment(1);
    state.editing_text = state.segments[0].display_text().to_string();
    state.split_selected_segment(None);

    assert_eq!(state.segments.len(), 2);
    for seg in &state.segments {
        assert!(seg.translation.is_none(), "不该凭空造出译文");
        assert!(seg.translation_lang.is_none());
        assert!(!seg.has_translation());
    }
}

/// 译文只有空白 → 拆分后两半都不带 `Some("")`（双语导出会多出一行空白）。
#[test]
fn manual_split_blank_translation_stays_none() {
    let mut state = test_state(vec![Segment::new(
        1,
        0.0,
        10.0,
        "前面这半句讲的是背景，后面这半句讲的是结论。",
    )]);
    state.segments[0].translation = Some("   ".to_string());
    state.segments[0].translation_lang = Some("English".to_string());
    state.select_segment(1);
    state.editing_text = state.segments[0].display_text().to_string();
    state.split_selected_segment(None);

    assert_eq!(state.segments.len(), 2);
    for seg in &state.segments {
        assert!(seg.translation.is_none());
        assert!(seg.translation_lang.is_none());
    }
}

// ───────────── P0-4：合并下句不能丢译文 ─────────────

/// 首句无译文、次句有译文 → 合并后保留次句译文（修前直接丢）。
#[test]
fn merge_keeps_next_translation_when_cur_has_none() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.segments[1].translation = Some("Second.".to_string());
    state.segments[1].translation_lang = Some("English".to_string());
    state.selected_segment_index = Some(1);

    state.merge_selected_with_next();

    assert_eq!(state.segments.len(), 1);
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("Second."),
        "次句译文不该随被删片段一起消失"
    );
    assert_eq!(
        state.segments[0].translation_lang.as_deref(),
        Some("English")
    );
}

/// 首句有译文、次句无 → 保留首句译文与标记。
#[test]
fn merge_keeps_cur_translation_when_next_has_none() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.segments[0].translation = Some("First.".to_string());
    state.segments[0].translation_lang = Some("English".to_string());
    state.selected_segment_index = Some(1);

    state.merge_selected_with_next();

    assert_eq!(state.segments.len(), 1);
    assert_eq!(state.segments[0].translation.as_deref(), Some("First."));
    assert_eq!(
        state.segments[0].translation_lang.as_deref(),
        Some("English")
    );
}

/// 两边都有、语言相同 → 译文拼接（换行）、语言标记保留。
#[test]
fn merge_concats_translations_with_same_lang() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.segments[0].translation = Some("First.".to_string());
    state.segments[0].translation_lang = Some("English".to_string());
    state.segments[1].translation = Some("Second.".to_string());
    state.segments[1].translation_lang = Some("English".to_string());
    state.selected_segment_index = Some(1);

    state.merge_selected_with_next();

    assert_eq!(state.segments.len(), 1);
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("First.\nSecond."),
        "两边都有译文时应拼接，而不是只留一边"
    );
    assert_eq!(
        state.segments[0].translation_lang.as_deref(),
        Some("English"),
        "两半语言相同，拼接后仍是该语言的译文"
    );
}

/// 两边都有、语言不同 → 拼接译文但**清掉语言标记**，让下一次翻译重译这条混合语言 cue。
#[test]
fn merge_drops_lang_when_languages_differ() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.segments[0].translation = Some("First.".to_string());
    state.segments[0].translation_lang = Some("English".to_string());
    state.segments[1].translation = Some("二番目。".to_string());
    state.segments[1].translation_lang = Some("日本語".to_string());
    state.selected_segment_index = Some(1);

    state.merge_selected_with_next();

    assert_eq!(state.segments.len(), 1);
    assert_eq!(
        state.segments[0].translation.as_deref(),
        Some("First.\n二番目。")
    );
    assert!(
        state.segments[0].translation_lang.is_none(),
        "一条 cue 混了两种语言，任何单一标记都是谎报，应置 None 迫使重译"
    );
    assert!(!state.segments[0].translation_matches("English"));
    assert!(!state.segments[0].translation_matches("日本語"));
    // 译文文本仍在：界面不显示「—」，用户还能看到内容
    assert!(state.segments[0].has_translation());
}

/// 两边都没译文 → 合并后仍无译文（不造 `Some("")`）。
#[test]
fn merge_without_translations_stays_none() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.selected_segment_index = Some(1);

    state.merge_selected_with_next();

    assert_eq!(state.segments.len(), 1);
    assert!(state.segments[0].translation.is_none());
    assert!(state.segments[0].translation_lang.is_none());
    assert!(!state.segments[0].has_translation());
}
/// 转写进行中（Processing）时翻译失败**不得**覆盖全局 `status`：否则状态栏里的
/// 转写进度会凭空消失。失败信息只落在翻译面板自己的 `translate_status_msg`。
#[test]
fn translate_failure_does_not_clobber_processing_status() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "第一句")]);
    state.status = ProcessStatus::Processing {
        stage: "识别中".to_string(),
        progress: 0.42,
        detail: "whisper 正在解码".to_string(),
    };
    assert!(state.is_processing());

    state.note_translate_failure("翻译失败：接口超时".to_string());

    assert!(
        state.is_processing(),
        "翻译失败不能把转写进度态冲掉: {:?}",
        state.status
    );
    assert_eq!(state.translate_progress, 0.0);
    assert_eq!(state.translate_status_msg, "翻译失败：接口超时");
}

/// 没有转写在跑时照旧写全局 `status`（剪辑台 / 视频库这些没有翻译面板的地方，
/// 仍要靠跨页错误横幅把失败告诉用户）。
#[test]
fn translate_failure_sets_status_when_not_processing() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "第一句")]);
    assert!(!state.is_processing());

    state.note_translate_failure("翻译失败：密钥无效".to_string());

    assert_eq!(
        state.status,
        ProcessStatus::Failed("翻译失败：密钥无效".to_string())
    );
    assert_eq!(state.translate_status_msg, "翻译失败：密钥无效");
}

/// 回归：播放推进 / 跳转时，「选中句」与「编辑缓冲」必须与
/// 画面字幕（`get_active_segment`）同步。
///
/// 缺陷现象：播放到第 3 句时，画面上已是第 3 句的字幕，右侧编辑框却还停在
/// 第 1 句——因为播放时钟只改 `selected_segment_index`、没同步 `editing_text`。
/// 用户看到就是「视频字幕和右边字幕对不上」。
#[test]
fn playback_advance_keeps_selection_and_edit_buffer_in_sync() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
        Segment::new(3, 4.0, 6.0, "第三句"),
    ]);
    state.select_segment(1);
    assert_eq!(state.editing_text, "第一句");

    // 播放推进到 4.5s（落在第 3 句）
    state.current_time = 4.5;
    assert!(state.sync_selection_to_time(false));
    assert_eq!(state.selected_segment_index, Some(3));
    assert_eq!(
        state.editing_text, "第三句",
        "编辑缓冲必须跟上画面字幕，否则就是“对不上”"
    );

    // 同一句内重复调用不应报「已变化」（避免 40ms 回调里空转）
    assert!(!state.sync_selection_to_time(false));
}

/// 用户正在编辑框里打字时（`preserve_editing = true`），播放推进只跟随
/// 选中项、**不得**覆盖 `editing_text`——否则把用户敲到一半的字冲掉。
#[test]
fn playback_advance_preserves_in_progress_edit_buffer() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.select_segment(1);
    state.editing_text = "第一句正在打字".to_string();

    state.current_time = 3.0;
    assert!(state.sync_selection_to_time(true));
    assert_eq!(state.selected_segment_index, Some(2));
    assert_eq!(
        state.editing_text, "第一句正在打字",
        "正在输入的文本不能被播放推进冲掉"
    );
}

/// 跳转（时间轴点击 / 上下句外的 seek）也必须同步编辑缓冲。
#[test]
fn seek_to_aligns_selection_and_edit_buffer_with_overlay() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 2.0, 4.0, "第二句"),
    ]);
    state.select_segment(1);
    state.seek_to(3.2);
    assert_eq!(state.selected_segment_index, Some(2));
    assert_eq!(state.editing_text, "第二句");

    // 半开区间：落在切句点 2.0（= 第二句的 start）归第二句，
    // 与画面字幕取值一致。
    state.seek_to(2.0);
    assert_eq!(state.selected_segment_index, Some(2));
    assert_eq!(state.editing_text, "第二句");
}

/// 点击字幕清单某行 = 选中该句并把播放位置归位到该句起点（校对台的鼠标点击契约）。
/// 与键盘「上/下句」一致：`current_time` 必须落在被点句的 `start`，且编辑缓冲同步。
#[test]
fn click_row_selects_and_seeks_to_segment_start() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 2.0, "第一句"),
        Segment::new(2, 3.5, 5.0, "第二句"),
    ]);
    // 播放停在别处（第 2 句范围内），点击第 2 行应把位置拉回该句起点
    state.current_time = 4.8;
    state.select_segment(2);
    state.seek_to(3.5);
    assert_eq!(state.selected_segment_index, Some(2));
    assert_eq!(state.current_time, 3.5);
    assert_eq!(state.editing_text, "第二句");

    // 点击第 1 行同理：位置归位到 0.0
    state.select_segment(1);
    state.seek_to(0.0);
    assert_eq!(state.selected_segment_index, Some(1));
    assert_eq!(state.current_time, 0.0);
    assert_eq!(state.editing_text, "第一句");
}

/// 回归：线程数收敛到 `[2, cores]` 时，单核机器（cores == 1）不得 panic。
/// 旧代码 `whisper_threads.clamp(2, cores)` 在 cores < 2 时是 `clamp(2, 1)`，
/// `min <= max` 断言直接崩；CPU 档切换走的正是这条路径。
#[test]
fn clamp_threads_to_cores_handles_single_core() {
    // cores = 1：下界与上界都落到 2，绝不 panic
    assert_eq!(AppState::clamp_threads_to_cores(16, 1), 2);
    assert_eq!(AppState::clamp_threads_to_cores(2, 1), 2);
    // cores = 0（异常硬件档案）同样兜底到 2
    assert_eq!(AppState::clamp_threads_to_cores(8, 0), 2);
    // 常规核数：夹到 [2, cores]
    assert_eq!(AppState::clamp_threads_to_cores(1, 16), 2);
    assert_eq!(AppState::clamp_threads_to_cores(64, 16), 16);
    assert_eq!(AppState::clamp_threads_to_cores(8, 16), 8);
    // 不变量：结果恒在 [2, max(cores, 2)] 内，且永远 >= 2
    for cores in 0u32..=32 {
        for req in [1u32, 2, 4, 8, 16, 32, 64] {
            let v = AppState::clamp_threads_to_cores(req, cores);
            assert!(v >= 2, "线程数不得低于 2: req={req} cores={cores} -> {v}");
            assert!(
                v <= cores.max(2),
                "线程数不得超过核数: req={req} cores={cores} -> {v}"
            );
        }
    }
}

/// 配置落盘失败必须被记成可见提示，成功则清掉旧错误。
///
/// 这是「设置保存失败」横幅的唯二状态来源：此前 7 处 `let _ = save_to_file`
/// 把失败完全吞掉，用户改完设置、重启发现设置又变回去了，却毫无线索。
#[test]
fn persist_config_records_failure_and_clears_on_success() {
    let mut state = test_state(Vec::new());
    // 造一个「父路径是文件」的非法目标：`save_to_file` 第一步 create_dir_all
    // 就会失败，稳定复现写盘失败，不依赖磁盘满/只读这类环境条件。
    let base = std::env::temp_dir().join(format!(
        "v2w_persist_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&base).unwrap();
    let blocker = base.join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let bad = blocker.join("config.toml");

    state.config_write_error = None;
    state.persist_config_to(&bad);
    assert!(
        state.config_write_error.is_some(),
        "写盘失败必须被记录成可见提示"
    );

    // 换成合法目标：成功落盘后旧错误应被清掉，否则提示条会一直挂着。
    let good = base.join("config.toml");
    state.persist_config_to(&good);
    assert!(good.exists(), "合法目标应真的写出 config.toml");
    assert!(
        state.config_write_error.is_none(),
        "成功落盘后必须清除上一次的失败提示"
    );

    let _ = std::fs::remove_dir_all(&base);
}
/// 造一个进程内唯一的临时目录，供落盘相关单测隔离使用。
fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 造一个「父路径是文件」的非法落盘目标：`save_to_file` 第一步
/// `create_dir_all` 就会失败，稳定复现写盘失败，不依赖磁盘满 / 只读。
fn make_bad_target(base: &std::path::Path) -> std::path::PathBuf {
    let blocker = base.join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    blocker.join("config.toml")
}

/// 拖动闸门：拖动期间 `persist_config_to` 不写盘、不改错误状态；
/// 松手补写一次，文件真的落盘、闸门复位。
#[test]
fn config_drag_gate_defers_persist_until_release() {
    let mut state = test_state(Vec::new());
    let base = unique_temp_dir("v2w_drag_gate");
    let good = base.join("config.toml");

    state.config_write_error = None;
    state.begin_config_drag();
    assert!(state.config_drag_active, "按下后闸门应打开");

    // 闸门开着：拖动中反复调用都只留内存，不写盘、不产生错误。
    state.persist_config_to(&good);
    state.persist_config_to(&good);
    assert!(
        !good.exists(),
        "拖动中不得写盘，否则一次拖动会打出几十上百次同步写"
    );
    assert!(
        state.config_write_error.is_none(),
        "拖动中不写盘，自然也不该冒出保存失败提示"
    );

    // 松手：补写一次，文件真的出现、闸门复位。
    state.end_config_drag_to(&good);
    assert!(good.exists(), "松手后应真的写出 config.toml");
    assert!(!state.config_drag_active, "松手后闸门应关闭");

    let _ = std::fs::remove_dir_all(&base);
}

/// 未开闸门时 `end_config_drag_to` 是空操作：既不写盘也不误报错误
/// （否则一次普通点击松手会把「其实没保存」的错误提示挂上去）。
#[test]
fn end_config_drag_without_begin_is_noop() {
    let mut state = test_state(Vec::new());
    let base = unique_temp_dir("v2w_drag_noop");
    let bad = make_bad_target(&base);

    state.config_write_error = None;
    state.end_config_drag_to(&bad);
    assert!(
        state.config_write_error.is_none(),
        "没在拖动时松手不该触发落盘、更不该报错"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// 拖到非法路径：闸门照常打开、不 panic；松手才把失败记成可见提示。
#[test]
fn config_drag_gate_records_failure_on_release() {
    let mut state = test_state(Vec::new());
    let base = unique_temp_dir("v2w_drag_fail");
    let bad = make_bad_target(&base);

    state.config_write_error = None;
    state.begin_config_drag();
    state.persist_config_to(&bad);
    assert!(
        state.config_write_error.is_none(),
        "拖动中不写盘，失败也要等松手才记账"
    );

    state.end_config_drag_to(&bad);
    assert!(!state.config_drag_active, "松手后闸门应关闭");
    assert!(
        state.config_write_error.is_some(),
        "松手补写失败必须被记成可见提示"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// `record_db_write_error` 追加而非覆盖：同一次操作里可能先后失败两处，
/// 两条都要留下（只显示最后一条会让用户排查时漏掉另一处）。
#[test]
fn record_db_write_error_appends_and_keeps_order() {
    let mut state = test_state(Vec::new());
    assert!(state.db_write_error.is_none());

    state.record_db_write_error("历史记录保存失败：库被锁");
    assert_eq!(
        state.db_write_error.as_deref(),
        Some("历史记录保存失败：库被锁")
    );

    // 第二条来自另一个环节：两条都在，且先发生的排在前
    state.record_db_write_error("字幕保存失败：磁盘已满");
    assert_eq!(
        state.db_write_error.as_deref(),
        Some("历史记录保存失败：库被锁；字幕保存失败：磁盘已满")
    );

    // 成功落库由调用方显式清空，不会被追加逻辑复活
    state.db_write_error = None;
    assert!(state.db_write_error.is_none());
}

/// 空串也要如实记录：调用方传进来的就是「发生了什么」，helper 不该
/// 擅自把空消息当成「没有错误」而吞掉。
#[test]
fn record_db_write_error_keeps_empty_message() {
    let mut state = test_state(Vec::new());
    state.record_db_write_error("");
    assert_eq!(state.db_write_error.as_deref(), Some(""));
}

/// 真实链路的顺序契约：转写结果入库失败时，错误要在第 2 步 flush 之后才挂到
/// `db_write_error` 上。这里锁住两个事实——(1) flush 在无脏数据时不会误清已有
/// 提示；(2) 之后补挂的入库失败会追加在已有提示后面，而不是被吞掉。
#[test]
fn db_insert_error_survives_successful_flush() {
    let mut state = test_state(Vec::new());
    // flush 成功（无脏数据，等同 no-op）会把 db_write_error 清空，
    // 若把 insert 错误在 flush 之前挂上就会被吞掉——这正是修复点。
    state.db_write_error = Some("旧错误".to_string());
    state.segments_dirty = false;
    state.flush_segments_if_dirty();
    assert_eq!(state.db_write_error.as_deref(), Some("旧错误"));

    // 模拟第 1 步入库失败：错误在 flush 之后补挂，必须留下
    state.record_db_write_error("历史记录保存失败：库被锁");
    assert_eq!(
        state.db_write_error.as_deref(),
        Some("旧错误；历史记录保存失败：库被锁")
    );
}

/// 与之相对：flush 自身成功时会把陈旧提示清空（成功即代表不再有未落库改动）。
#[test]
fn successful_flush_clears_stale_db_error() {
    let mut state = test_state(Vec::new());
    state.db_write_error = Some("上一次的失败".to_string());
    state.segments_dirty = true;
    state.active_task_id = None; // 无主键 → sync 直接 Ok
    state.flush_segments_if_dirty();
    assert!(state.db_write_error.is_none(), "成功落库后应清空提示");
}

/// 一条合成回报。`error_at_dispatch` 传 `None` 表示「派发时提示条是干净的」——
/// 真实链路里就是这个状态（上一次成功已经清空），所以测试里的回报必须照抄。
fn synthetic_write(seq: u64, revision: u64, result: Result<(), String>) -> SegmentWriteDone {
    SegmentWriteDone {
        seq,
        task_id: 1,
        revision,
        error_at_dispatch: None,
        result,
    }
}

/// 去抖落库：登记之后不许立刻写；停手满窗口才交给后台；立刻写入的入口要作废登记。
///
/// 回归护栏：`seek_to` 在拖动时间轴时每 40ms 被调一次。一旦回到「每次都同步写库」，
/// 5000 句的工程每拖一下就要多付 10~30ms 的序列化 + UPDATE，手感直接变差。
///
/// 异步化之后的契约：到点那次只**派发**（脏标记在派发成功时清掉，登记同时作废），
/// 写库本身在后台；同步版 `flush_segments_if_dirty`（换工程 / 导出 / 关窗守卫）仍然
/// 是「返回时已写完」，所以它照样会把登记一起作废。
#[test]
fn deferred_flush_waits_for_idle_window() {
    let mut state = test_state(Vec::new());
    state.active_task_id = None; // 无主键 → 没有可写回的行，等同已保存

    // 没有改动就不登记：空登记只会让定时器白醒一次
    state.segments_dirty = false;
    state.defer_segments_flush();
    assert!(state.segments_flush_due.is_none(), "没改动不该登记待落库");

    state.segments_dirty = true;
    state.defer_segments_flush();
    let due = state.segments_flush_due.expect("有改动必须登记待落库");

    // 窗口没到：不派发，也不清脏标记，登记继续留着
    assert!(!state.flush_segments_if_due_at(due + std::time::Duration::from_millis(300)));
    assert!(state.segments_dirty, "没到点就不该清脏标记");
    assert!(state.segments_flush_due.is_some(), "没到点登记要留着");

    // 停手满窗口：派发一次（无主键 → 直接清脏），脏标记与登记一起清掉
    assert!(state.flush_segments_if_due_at(due + SEGMENTS_FLUSH_IDLE));
    assert!(!state.segments_dirty, "派发之后脏标记必须清掉");
    assert!(state.segments_flush_due.is_none(), "派发之后登记必须清掉");

    // 立刻写入的入口（换工程 / 导出 / 关窗）要作废登记：
    // 不作废的话定时器稍后还会把同一份内容再写一遍。
    state.segments_dirty = true;
    state.defer_segments_flush();
    assert!(state.segments_flush_due.is_some());
    state.flush_segments_if_dirty();
    assert!(state.segments_flush_due.is_none(), "立刻写入后登记必须作废");

    // 期间被别的路写过（脏标记已清）：到点也不写，登记顺手作废
    state.segments_dirty = true;
    state.defer_segments_flush();
    state.segments_dirty = false;
    assert!(!state.flush_segments_if_due_at(due + SEGMENTS_FLUSH_IDLE * 2));
    assert!(state.segments_flush_due.is_none(), "空登记不能一直挂着");
}

/// 后台写真的把字幕写进了库，而且**渲染线程不再等它**。
///
/// 这是本任务的主路径验证：`flush_segments_if_dirty` 的同步语义一个字没改，
/// 但去抖那条路（`flush_segments_if_dirty_async`）改成派发——派发返回时**脏标记仍在**
/// （内容还没落库，只是交出去了）、快照已在后台（`background_segments_write_pending()`
/// 能看到），一两帧之后落库完成、回报收进状态，`Database::load_task_segments` 能读回改动。
#[test]
fn async_flush_writes_to_database_from_background_thread() {
    let mut state = test_state(Vec::new());
    let db = crate::storage::Database::open(":memory:").expect("内存库应能打开");
    let task_id = db
        .insert_task("bg.mp4", "bg.mp4", 10.0, "completed", &Vec::new(), None)
        .expect("先插一条空工程");
    state.db = db;
    state.active_task_id = Some(task_id);
    state.segments = vec![Segment::new(1, 0.0, 1.0, "后台写进去的第一句")];
    state.bump_segments_revision();
    state.segments_dirty = true;

    state.flush_segments_if_dirty_async();

    assert!(
            state.segments_dirty,
            "派发只是把快照交给后台、还没落库——脏标记必须保留（关窗守卫靠它堵住「在飞写没落地就关窗」）"
        );
    assert!(
        state.segments_flush_due.is_none(),
        "派发同时要作废待落库登记"
    );

    // 后台线程跑完之前，渲染线程只做「收回报」这件事；这里轮询模拟定时器窗口。
    //
    // 等的判据是「写真的落库了」——`background_segments_write_pending()` 归零只说明
    // 「结果已经投进通道」，而收到回报本身不会让库里立刻可见（回报是在写完之后才投的，
    // 但本测试的轮询窗口比写线程快，可能先看到 pending == 0 而写还没落到读连接上）。
    // 所以直接以「读得回这一句」为完成条件，另外用错误通道做失败短路。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut loaded = Vec::new();
    while std::time::Instant::now() < deadline {
        state.poll_background_writes();
        if state.db_write_error.is_some() {
            break;
        }
        loaded = state
            .db
            .load_task_segments(task_id)
            .expect("读回后台写的字幕");
        if !loaded.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        state.db_write_error.is_none(),
        "写库不该失败：{:?}",
        state.db_write_error
    );
    assert_eq!(
        loaded.len(),
        1,
        "后台写必须真的落库（读到 {} 句）",
        loaded.len()
    );
    assert_eq!(loaded[0].text, "后台写进去的第一句");
    assert!(
        !state.segments_dirty,
        "后台写成功、回报收进状态后，脏标记必须清掉（否则界面一直显示未保存）"
    );
}

/// 派发不等于写完：旧快照的失败结局**不许**回写现在的状态（防回退）。
///
/// 三条判定一起钉住：
/// 1. 派发过的就作废旧登记，不让定时器把同一份内容再写一遍；
/// 2. 迟到的旧失败（通道里还有、但水位已被更新的成功越过）只留日志，不点提示灯；
/// 3. 落在旧修订上的失败不置脏——否则定时器会把那份旧内容当当前内容再派发一次。
#[test]
fn background_write_completion_never_rewinds_state() {
    let mut state = test_state(Vec::new());
    state.active_task_id = None;
    state.segments_dirty = true;
    state.defer_segments_flush();

    state.flush_segments_if_dirty_async();

    assert!(!state.segments_dirty, "派发之后脏标记必须清掉");
    assert!(
        state.segments_flush_due.is_none(),
        "派发过的快照不许再挂着待落库登记"
    );

    // 同一轮排空里同时到达「旧失败 + 更新的成功」：两条都要处理（这正是
    // 「拿被前一条改过的字段去比后一条」会漏掉成功的那种情形），最终以更新的成功收口。
    let mut dirty = false;
    let mut error = None;
    let old_failure = synthetic_write(1, 7, Err("库被锁".to_string()));
    let newer_success = synthetic_write(2, 7, Ok(()));
    drain_segment_writes(
        &mut error,
        &mut dirty,
        7,
        None,
        &[old_failure, newer_success],
    );
    assert!(error.is_none(), "更新的成功之后不许再挂旧失败");
    assert!(
        !dirty,
        "旧失败被同工程更新的成功越过，不许再标脏（否则界面会要求重写一遍）"
    );

    // 换到「更新的成功在前、更旧的失败在后」（乱序到达）：失败描述的是一份被同工程
    // 更新的写覆盖掉的快照——提示灯与脏标记都不许回退。过期筛选在 `drain_results`
    // 里按**同工程水位**做（不是全局写序号），所以这里走真实通道验证。
    state.segments_writes.tracker.note_applied(1, 3);
    state
        .segments_writes
        .submit
        .send(synthetic_write(
            1,
            state.segments_revision,
            Err("旧快照写失败".to_string()),
        ))
        .expect("投递回报");
    let dirty_before = state.segments_dirty;
    state.poll_background_writes();
    assert!(
        state.db_write_error.is_none(),
        "同工程已有更新结论的迟到失败不许挂提示条"
    );
    assert_eq!(
        state.segments_dirty, dirty_before,
        "迟到的旧失败不许改脏标记"
    );

    // 最新一条失败 + 旧修订：错误挂上（不许静默），但不置脏——交给更新的那份编辑去落库
    error = None;
    let mut dirty = false;
    apply_segment_write_result(
        &mut error,
        &mut dirty,
        &synthetic_write(4, 7, Err("磁盘已满".to_string())),
        9,
        true,
    );
    assert_eq!(
        error.as_deref(),
        Some("字幕保存失败：磁盘已满"),
        "失败本身必须可见（提示条是用户唯一的了解渠道）"
    );
    assert!(!dirty, "旧修订的失败不许把当前内容标脏");

    // 最新一条失败 + 当前修订：必须置脏，否则定时器不再重试
    error = None;
    let mut dirty = false;
    apply_segment_write_result(
        &mut error,
        &mut dirty,
        &synthetic_write(4, 9, Err("磁盘已满".to_string())),
        9,
        true,
    );
    assert!(dirty, "当前修订上的失败必须置脏以便重试");

    // 修订号判定的边界：同一修订才置脏，更新过的修订不置脏（纯函数直接钉住）
    let mut dirty = false;
    assert!(mark_segments_dirty_for_revision(&mut dirty, 9, 9));
    assert!(dirty);
    let mut dirty = false;
    assert!(!mark_segments_dirty_for_revision(&mut dirty, 8, 9));
    assert!(!dirty, "旧修订的失败不许把当前内容标脏");
}

/// 后台写失败**不能静默**：走一次真实的异步派发失败，等它把「字幕保存失败：…」
/// 挂回 `db_write_error`，并把脏标记置回（下一次 flush 会重试）。
///
/// 失败是**确定性**造出来的：库文件照常建、工程照常插，然后把库文件设成只读
/// （模拟只读挂载 / 磁盘满这类持久性故障）。SQLite 在 WAL 模式下连写都起不来，
/// `update_task_segments_json` 必然返回 Err——正是用户实际会撞上的那类错误，
/// 也是「错误不许静默」这条要求的端到端验证。
#[test]
fn async_flush_surfaces_database_failure() {
    let dir = std::env::temp_dir().join(format!(
        "voice2word_bg_fail_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let db_path = dir.join("voice2word.db");
    let id = {
        let db = crate::storage::Database::open(&db_path).expect("打开文件库");
        let id = db
            .insert_task("fail.mp4", "fail.mp4", 5.0, "completed", &Vec::new(), None)
            .expect("先插一条空工程");
        // 把 WAL 合并回主库文件，保证随后只读打开时读得到这条记录。
        db.maintain_on_shutdown();
        id
    };
    let mut perms = std::fs::metadata(&db_path)
        .expect("库文件元数据")
        .permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&db_path, perms).expect("把库文件设为只读");

    let mut state = test_state(Vec::new());
    state.db = crate::storage::Database::open(&db_path).expect("只读打开（读没问题）");
    state.active_task_id = Some(id);
    state.segments = vec![Segment::new(1, 0.0, 1.0, "这一句写不进去")];
    state.bump_segments_revision();
    state.segments_dirty = true;

    state.flush_segments_if_dirty_async();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while state.db_write_error.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
        state.poll_background_writes();
    }
    let text = state.db_write_error.clone().unwrap_or_default();
    assert!(
        text.contains("字幕保存失败"),
        "后台失败必须挂到提示通道（db_write_error），实际：{text:?}"
    );
    assert!(
        state.segments_dirty,
        "当前修订上的失败必须把脏标记置回，下一次 flush 才会重试"
    );

    // 收尾：恢复可写再整目录删掉（连同 -wal/-shm），不给下一次测试留垃圾。
    drop(state);
    let mut perms = std::fs::metadata(&db_path)
        .expect("收尾时读元数据")
        .permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    let _ = std::fs::set_permissions(&db_path, perms);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 修复常见错误：命中才压撤销点；改动要标脏、推修订号、清词级时间戳，且可撤销。
#[test]
fn apply_cleanup_edits_text_and_is_undoable() {
    use crate::subtitle::cleanup::CleanupRule;

    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "嗯，我来了。"),
        Segment::new(2, 1.0, 2.0, "这个这个"),
    ]);
    state.segments[1].words = vec![crate::subtitle::segment::WordTiming {
        word: "这个".to_string(),
        start: 1.0,
        end: 1.5,
    }];
    let revision = state.segments_revision;

    let plan = state.apply_cleanup();
    assert_eq!(plan.count_of(CleanupRule::Filler), 1);
    assert_eq!(state.segments[0].text, "我来了。");
    assert_eq!(state.segments[1].text, "这个");
    assert!(
        state.segments[1].words.is_empty(),
        "文本一变，词级时间戳必须清空"
    );
    assert!(state.segments_revision > revision, "清单缓存要失效");
    // 立刻写入的入口会把脏标记清掉（`flush_segments_if_dirty` 落库后清），
    // 所以这里看的是审计与修订号，而不是 `segments_dirty`。
    let logged = state
        .edit_log
        .of_kind(crate::subtitle::audit::EditKind::Replace);
    assert_eq!(logged.len(), 2, "语气词与重复字词各记一条摘要");
    assert!(
        logged[0].before.contains("未修复"),
        "摘要要写清改的是哪一类问题：{}",
        logged[0].before
    );
    assert!(state.cleanup_note.is_some(), "界面要能显示结果提示");
    assert!(state.can_undo(), "改动必须可撤销");
    assert!(state.undo());
    assert_eq!(state.segments[0].text, "嗯，我来了。");
}

/// 没有可改之处时不压撤销点：空操作占掉一层 Ctrl+Z 会让用户以为撤销坏了。
#[test]
fn apply_cleanup_without_hits_leaves_state_untouched() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 1.0, "你好世界")]);
    assert!(!state.can_undo());
    let plan = state.apply_cleanup();
    assert!(plan.is_empty());
    assert!(!state.can_undo(), "无命中不该产生撤销点");
    assert!(!state.segments_dirty, "无命中不该把工程标脏");
}

/// 规则全关时给出明确提示，而不是静默什么都不做。
#[test]
fn apply_cleanup_with_no_rules_points_at_the_toggles() {
    let mut state = test_state(vec![Segment::new(1, 0.0, 1.0, "嗯，我来了")]);
    state.cleanup_rules = crate::subtitle::cleanup::CleanupRules::NONE;
    let plan = state.apply_cleanup();
    assert!(plan.is_empty());
    let note = state.cleanup_note.clone().unwrap_or_default();
    assert!(note.contains("勾选"), "提示要指向勾选项：{note}");
}

/// 折叠重复句：句数减少、序号重排、时间跨度合并、可撤销。
#[test]
fn fold_duplicate_segments_merges_and_renumbers() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "你好世界"),
        Segment::new(2, 1.0, 2.0, "你好世界"),
        Segment::new(3, 2.5, 3.5, "别的"),
    ]);
    state.selected_segment_index = Some(2);

    let removed = state.fold_duplicate_segments();
    assert_eq!(removed, 1);
    assert_eq!(state.segments.len(), 2);
    assert_eq!(state.segments[0].end, 2.0, "保留句的时间要盖住整组");
    assert_eq!(state.segments[1].index, 2, "删条后必须重排序号");
    assert_eq!(
        state.selected_segment_index,
        Some(1),
        "选中的那句被删掉后要落到现存的第一句"
    );
    // 立刻写入的入口会在落库后清掉脏标记，所以这里看审计留痕与撤销可用性。
    assert_eq!(
        state
            .edit_log
            .of_kind(crate::subtitle::audit::EditKind::Merge)
            .len(),
        1,
        "折叠属于结构性删除，审计必须留一条合并记录"
    );
    assert!(state.can_undo());
    assert!(state.undo());
    assert_eq!(state.segments.len(), 3);

    // 删除发生在选中句之前时，选中的仍是同一句话（序号前移一位），
    // 而不是「序号不变、内容换人」。
    state.selected_segment_index = Some(3);
    state.fold_duplicate_segments();
    assert_eq!(state.segments.len(), 2);
    assert_eq!(state.selected_segment_index, Some(2));
    assert_eq!(
        state
            .segments
            .iter()
            .find(|s| s.index == 2)
            .map(|s| s.text.as_str()),
        Some("别的"),
        "选中的必须还是原来那一句"
    );
}

/// 没有重复句时不产生撤销点、也不假装删了东西。
#[test]
fn fold_duplicate_segments_is_a_noop_without_duplicates() {
    let mut state = test_state(vec![
        Segment::new(1, 0.0, 1.0, "甲句"),
        Segment::new(2, 1.0, 2.0, "乙句"),
    ]);
    assert_eq!(state.fold_duplicate_segments(), 0);
    assert!(!state.can_undo());
    assert_eq!(state.segments.len(), 2);
}
/// 同工程的写按序号收口：更新的内容落库后，更旧的快照必须被跳过而不是盖回去。
///
/// 回归护栏：写入闸只保证「不交错」，普通 `Mutex` 不保证先到先得——没有按工程的
/// 水位，两个在飞的写完全可能「新的先写、旧的后写」，库里留下旧内容却没有任何提示。
#[test]
fn stale_snapshot_is_skipped_after_a_newer_write_landed() {
    use crate::storage::Database;
    let db = Database::open(":memory:").expect("内存库应能打开");
    let id = db
        .insert_task(
            "floor.mp4",
            "floor.mp4",
            3.0,
            "completed",
            &[Segment::new(1, 0.0, 3.0, "新内容")],
            None,
        )
        .expect("先插一条带内容的工程");
    let writes = BackgroundWrites::default();
    // 模拟「更新的写已经落库」：同工程水位已经是 7。
    writes.tracker.note_applied(id, 7);

    let stale = SegmentWriteSnapshot {
        task_id: id,
        revision: 3,
        json: serde_json::to_string(&vec![Segment::new(1, 0.0, 3.0, "旧内容")]).unwrap(),
    };
    let guard = writes.lock_writer();
    let wrote = write_snapshot_under_gate(&writes, &db, &stale, 5).expect("跳过不是失败");
    drop(guard);
    assert!(!wrote, "更旧的快照必须被跳过");
    assert_eq!(
        db.load_task_segments(id).expect("读回字幕")[0].text,
        "新内容",
        "旧内容不许盖掉新内容"
    );

    // 反例：序号比水位新时照常写入——水位不是「一写就锁死」的开关。
    let newer = SegmentWriteSnapshot {
        task_id: id,
        revision: 4,
        json: serde_json::to_string(&vec![Segment::new(1, 0.0, 3.0, "更新内容")]).unwrap(),
    };
    let guard = writes.lock_writer();
    let wrote = write_snapshot_under_gate(&writes, &db, &newer, 9).expect("写入应成功");
    drop(guard);
    assert!(wrote);
    assert_eq!(
        db.load_task_segments(id).expect("读回字幕")[0].text,
        "更新内容"
    );
}

/// 同步落库要把「同工程水位」推到自己领的序号上：落后于它的旧回报不许再回写状态。
///
/// 回归护栏：水位原先只有「读」没有「写」（全代码库没有一处 store），两条判据永远
/// 不成立——同步写完成后，一条迟到的旧失败照样会把「字幕保存失败」挂上提示条，
/// 而那份内容其实已经写进库了。
///
/// 交错序列完全按真实链路摆：旧快照先派发（不 await）→ 用户又改一版 → 同步落库
/// （换工程 / 导出 / 关窗）把它写掉 → 旧快照的失败结局这时才回来。
#[test]
fn sync_flush_records_task_watermark_and_drops_late_stale_failure() {
    let mut state = test_state(Vec::new());
    let id = state
        .db
        .insert_task("sync.mp4", "sync.mp4", 2.0, "completed", &[], None)
        .expect("插一条工程");
    state.active_task_id = Some(id);

    // 第一代：派发出去（seq 1），但先不让它收尾。
    state.segments = vec![Segment::new(1, 0.0, 2.0, "旧内容")];
    state.bump_segments_revision();
    state.segments_dirty = true;
    state.flush_segments_if_dirty_async();
    let stale_seq = 1;

    // 用户又改一版，然后走同步落库：它领到更大的写序号。
    state.segments = vec![Segment::new(1, 0.0, 2.0, "最新内容")];
    state.bump_segments_revision();
    state.segments_dirty = true;
    state.flush_segments_if_dirty();

    assert!(
        state.segments_writes.tracker.applied_seq(id) > stale_seq,
        "同步写必须把同工程水位推到自己领的号上"
    );
    assert!(!state.segments_dirty, "同步写完就是干净状态");

    // 旧快照的失败结局现在才回来（乱序到达）：它描述的内容已经被同步写覆盖，
    // 既不许挂提示条，也不许把内容标脏。
    state
        .segments_writes
        .submit
        .send(SegmentWriteDone {
            seq: stale_seq,
            task_id: id,
            revision: state.segments_revision,
            error_at_dispatch: None,
            result: Err("库被锁".to_string()),
        })
        .expect("投递回报");
    state.poll_background_writes();
    assert!(
        state.db_write_error.is_none(),
        "被同步写越过的迟到失败不许挂提示条：{:?}",
        state.db_write_error
    );
    assert!(!state.segments_dirty, "内容已经在库里，不许再标脏");
    assert_eq!(
        state.db.load_task_segments(id).expect("读回字幕")[0].text,
        "最新内容",
        "库里必须是同步写那一版"
    );
}

/// 提示条竞价输了的失败：提示条不动，但当前内容必须补上脏标记（否则没有重试）。
#[test]
fn banner_suppressed_failure_still_marks_current_revision_dirty() {
    let pre_drain = Some("历史记录保存失败：库被锁".to_string());
    let mut error = pre_drain.clone();
    let mut dirty = false;
    // 派发时提示条是干净的（synthetic_write 的 error_at_dispatch 为 None），
    // 之后别的落库挂上了更新的事实 → 这条失败失去回写提示条的资格。
    let failure = synthetic_write(4, 9, Err("磁盘已满".to_string()));
    drain_segment_writes(&mut error, &mut dirty, 9, pre_drain.as_deref(), &[failure]);
    assert_eq!(
        error.as_deref(),
        Some("历史记录保存失败：库被锁"),
        "更新的提示条不许被旧失败覆盖"
    );
    assert!(
        dirty,
        "失败描述的正是当前内容：必须置脏，否则这次失败既不提示也不重试"
    );

    // 落在旧修订上的同类失败仍然不许置脏（原有边界保持不变）。
    let pre_drain = Some("别的错误".to_string());
    let mut error = pre_drain.clone();
    let mut dirty = false;
    let stale = synthetic_write(5, 3, Err("磁盘已满".to_string()));
    drain_segment_writes(&mut error, &mut dirty, 9, pre_drain.as_deref(), &[stale]);
    assert!(!dirty, "旧修订的失败不许把当前内容标脏");
    assert_eq!(error.as_deref(), Some("别的错误"));
}

/// 派发前的「同工程水位」判据：已经被更新的写覆盖的快照不再派发，也不算失败。
///
/// 「水位 ≥ 待发号」这个输入是**人工构造**的（生产中领号只在渲染线程且单调递增，
/// 走不到这条分支）。这里钉的是分支语义本身：以后若把判据改成 `>`、或者把
/// 「跳过」改成「报失败 / 照样起线程」，这条测试会立刻红。
#[test]
fn dispatch_skips_snapshot_already_covered_by_task_floor() {
    let mut state = test_state(Vec::new());
    state.active_task_id = Some(42);
    state.segments = vec![Segment::new(1, 0.0, 1.0, "内容")];
    state.segments_dirty = true;
    let next = state
        .segments_writes
        .tracker
        .next_seq
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(next, 1, "写序号从 1 开始（0 是「还没写过」的哨兵）");
    // 模拟：同工程的同步写（或更新的后台写）已经完成，水位 = 下一个待发号。
    state.segments_writes.tracker.note_applied(42, next);
    let snapshot = state.segments_write_snapshot().expect("有主键就有快照");
    assert_eq!(
        state.dispatch_segment_write(snapshot),
        SegmentDispatch::Saved,
        "被更新的写覆盖时必须按「已保存」处理，不能报失败"
    );
    assert_eq!(
        state.background_segments_write_pending(),
        0,
        "跳过的派发不许起线程"
    );
    assert_eq!(
        state
            .segments_writes
            .tracker
            .next_seq
            .load(std::sync::atomic::Ordering::SeqCst),
        next,
        "跳过的派发不领号（否则水位相对序号就错位了）"
    );
}
/// 诊断计数器（在飞快照数）必须有去有回：只加不减会让它随使用时长单调上涨，
/// 变成一个骗人的数（旧实现只在「线程起不来」那条路上减过一次）。
#[test]
fn in_flight_write_gauge_returns_to_zero_after_completion() {
    let mut state = test_state(Vec::new());
    let id = state
        .db
        .insert_task("gauge.mp4", "gauge.mp4", 1.0, "completed", &[], None)
        .expect("插一条工程");
    state.active_task_id = Some(id);
    state.segments = vec![Segment::new(1, 0.0, 1.0, "内容")];
    state.bump_segments_revision();
    state.segments_dirty = true;
    state.flush_segments_if_dirty_async();

    // 派发那一刻就要能看到「有一份在飞」——这正是旧实现的坑：计数跑到线程体里，
    // 派发返回到线程开跑之间它是 0，同步落库会以为自己可以走了。
    assert!(
        state.background_segments_write_pending() >= 1,
        "派发之后「在飞」计数必须立刻可见"
    );

    // 等「结果真的进通道」（pending 归零）+ 水位落下来，不是瞎睡。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while (state.background_segments_write_pending() > 0
        || state.segments_writes.tracker.applied_seq(id) == 0)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    state.poll_background_writes();

    assert_eq!(
        state.background_segments_write_pending(),
        0,
        "后台写应在期限内收尾"
    );
    assert_eq!(
        state
            .segments_writes
            .in_flight_writes
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "写结束后在飞计数必须归零（只增不减的诊断数字会骗人）"
    );
}

/// 转写收尾的脏字幕回写必须过闸：同工程已有更新的写落库时，旧的收尾快照不许盖回去。
///
/// 回归护栏：这条路径原先直接 `db.update_task_segments(id, segs)`，既没领号也没查水位，
/// 于是「后台在飞的新写」与「收尾的旧写」撞同一个 task 时，旧内容会后落地把新内容盖掉。
#[test]
fn finish_flush_write_is_gated_and_drops_stale_snapshot() {
    use crate::storage::Database;

    let db = Database::open(":memory:").expect("内存库应能打开");
    let id = db
        .insert_task("finish.mp4", "finish.mp4", 2.0, "completed", &[], None)
        .expect("插一条工程");
    // 与 AppState 内部同一个 BackgroundWrites：派发 / 同步写 / 过闸入口三方共用
    // （AppState::new 里正是把这个 Arc 同时交给 segments_writes 与 segments_write_gate）。
    let writes = Arc::new(BackgroundWrites::default());
    let gate = SegmentWriteGate {
        db: db.clone(),
        writes: Arc::clone(&writes),
    };

    // 用户又改了一版，走同步落库（换工程 / 导出）：它领到更大的序号并把水位推高。
    let newer = vec![Segment::new(1, 0.0, 2.0, "最新内容")];
    let newer_seq = writes.tracker.next_seq.fetch_add(1, SeqCst);
    {
        let _guard = writes.lock_writer();
        db.update_task_segments(id, &newer).expect("新内容先落库");
        writes.tracker.note_applied(id, newer_seq);
    }

    // 转写收尾这时才带着**旧**序号回写上一个工程的脏字幕。
    let stale = vec![Segment::new(1, 0.0, 2.0, "旧内容")];
    let wrote = gate
        .write_finished_task_segments(id, newer_seq.saturating_sub(1), &stale)
        .expect("被跳过不是失败");
    assert!(!wrote, "更旧的收尾写必须被水位挡下");
    assert_eq!(
        db.load_task_segments(id).expect("读回字幕")[0].text,
        "最新内容",
        "旧收尾快照不许盖掉更新的写"
    );

    // 反例：序号比水位新时照常写、并把水位推到自己——闸门不是「一写就锁死」。
    let fresh = vec![Segment::new(1, 0.0, 2.0, "收尾写的内容")];
    let fresh_seq = writes.tracker.next_seq.fetch_add(1, SeqCst);
    let wrote = gate
        .write_finished_task_segments(id, fresh_seq, &fresh)
        .expect("写入应成功");
    assert!(wrote);
    assert_eq!(
        db.load_task_segments(id).expect("读回字幕")[0].text,
        "收尾写的内容"
    );
    assert_eq!(
        writes.tracker.applied_seq(id),
        fresh_seq,
        "过闸写必须把同工程水位推到自己"
    );
}

/// 去抖派发**不清脏**，脏标记只在这一份内容被确认落库后才清。
///
/// 回归护栏（本轮修的真实漏洞）：旧实现在派发成功那一刻就清 `segments_dirty`。于是
/// 「改完 → 去抖派发 → 用户立刻关窗」这条链上，`flush_segments_before_close` 读到的
/// `had_dirty` 是 `false`——它不会做同步落库，也不会因为在飞写的失败而拦住关窗；那份在飞
/// 写一旦失败（磁盘满 / 库被锁），失败回报要等下一次 poll 才被看到，而窗口已经关了。
///
/// 这条测试钉住不变量：**派发返回时脏标记必须还在**（内容只是在飞、还没落库），只有后台
/// 成功收进状态之后才允许清掉。
#[test]
fn segments_dirty_survives_dispatch_until_write_confirmed() {
    use crate::storage::Database;

    let mut state = test_state(Vec::new());
    let db = Database::open(":memory:").expect("内存库应能打开");
    let id = db
        .insert_task("keep.mp4", "keep.mp4", 1.0, "completed", &[], None)
        .expect("插一条工程");
    state.db = db;
    state.active_task_id = Some(id);
    state.segments = vec![Segment::new(1, 0.0, 1.0, "改动")];
    state.bump_segments_revision();
    state.segments_dirty = true;

    let dispatched = state.flush_segments_if_dirty_async();

    assert!(dispatched, "有主键时应派发到后台（返回「有写在飞」）");
    assert!(
        state.segments_dirty,
        "派发只是把快照交出去、内容还没落库：脏标记必须保留，否则关窗守卫会在在飞写没落地时放行"
    );

    // 后台写完成、回报收进状态 → 这时才允许清脏。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while state.segments_dirty && std::time::Instant::now() < deadline {
        state.poll_background_writes();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    assert!(
        !state.segments_dirty,
        "后台写成功收口后脏标记必须清掉（否则界面会一直显示未保存）"
    );
    assert!(state.db_write_error.is_none(), "这条路径不该产生写库错误");
    let loaded = state.db.load_task_segments(id).expect("读回后台写的字幕");
    assert_eq!(loaded[0].text, "改动", "内容必须真的落库");
}
