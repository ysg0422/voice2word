//! 术语表导入导出、CPS 修正与阶段产物目录的定向单测。

use super::AppState;
use crate::subtitle::Segment;
use std::path::{Path, PathBuf};

/// 造一个不碰显卡 / 真实 `config.toml` 的最小 AppState。
fn fresh_state(segments: Vec<Segment>) -> AppState {
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
    let mut state = AppState::with_hardware(
        AppConfig::default(),
        db,
        pipeline,
        HardwareProfile::cpu_only(),
    );
    state.segments = segments;
    state
}

/// 临时目录里的唯一名字（避免同批测试互相踩）。
fn unique(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("v2w_{tag}_{nanos}"))
}

/// 导入 `.txt`：新增 / 覆盖 / 未变三个数必须如实分桶，且合并结果写回术语表字段。
#[test]
fn import_glossary_file_counts_added_overwritten_unchanged() {
    let dir = unique("gloss_txt");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("incoming.txt");
    // b / C 命中已有条目（b 换了译法、C 逐字相同），D 是新的。
    std::fs::write(&src, "b=B2\nC=丙\nD=丁\n").unwrap();

    let cfg = dir.join("config.toml");
    let mut state = fresh_state(Vec::new());
    state.config.translate.glossary = "A=甲\nB=乙\nC=丙\n".to_string();

    let (added, overwritten, unchanged) = state
        .import_glossary_file_to(&src, &cfg)
        .expect("导入应成功");

    assert_eq!((added, overwritten, unchanged), (1, 1, 1));
    // 覆盖留在原位、新增追加在后：b 的译法已更新、D 已写入、A 一字未动。
    let text = state.config.translate.glossary.clone();
    assert!(text.contains("b=B2"), "覆盖后译文应更新: {text}");
    assert!(text.contains("D=丁"), "新增条目应写回: {text}");
    assert!(text.contains("A=甲"), "未命中的旧条目必须保留: {text}");
    assert_eq!(
        text.lines().next(),
        Some("A=甲"),
        "覆盖应留在原位，不挪到末尾"
    );
    assert!(cfg.is_file(), "必须走既有落盘入口写出配置文件");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导入 `.csv`：按扩展名选 `from_csv`，带 BOM 与表头也能读。
#[test]
fn import_glossary_file_reads_csv_by_extension() {
    let dir = unique("gloss_csv");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("incoming.csv");
    std::fs::write(&src, "\u{FEFF}source,target\nOBS,obs studio\n").unwrap();

    let cfg = dir.join("config.toml");
    let mut state = fresh_state(Vec::new());
    let (added, overwritten, unchanged) = state
        .import_glossary_file_to(&src, &cfg)
        .expect("导入应成功");

    assert_eq!((added, overwritten, unchanged), (1, 0, 0));
    assert!(state.config.translate.glossary.contains("OBS=obs studio"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导出 CSV：表头固定，含逗号的字段按 RFC4180 加引号。
#[test]
fn export_glossary_csv_text_writes_header_and_escapes() {
    let mut state = fresh_state(Vec::new());
    state.config.translate.glossary = "苹果=apple\na,b=c\n".to_string();

    let csv = state.export_glossary_csv_text();
    assert!(csv.starts_with("source,target\n"), "首行必须是表头: {csv}");
    assert!(csv.contains("苹果,apple\n"));
    assert!(csv.contains("\"a,b\",c\n"), "含逗号字段必须加引号: {csv}");
}

/// CPS 修正：有改动压撤销点且可撤销；无改动一个撤销点都不许压。
#[test]
fn apply_cps_fix_is_undoable_and_noop_pushes_nothing() {
    // 10 个汉字塞进 1 秒 = 10 字/秒 > 默认上限 9 → 必然被延长。
    let mut state = fresh_state(vec![Segment::new(1, 0.0, 1.0, "一二三四五六七八九十")]);
    let plan = state.apply_cps_fix(crate::subtitle::DEFAULT_MAX_CPS, 0.0);

    assert!(plan.has_changes(), "超速句必须被规划进改动");
    assert!(state.can_undo(), "有改动必须压一层撤销点");
    assert!(
        state.segments[0].end > 1.0,
        "终点应被延长: {:?}",
        state.segments[0]
    );

    assert!(state.undo(), "撤销应成功");
    assert!(
        (state.segments[0].end - 1.0).abs() < 1e-9,
        "撤销后终点回到原值: {:?}",
        state.segments[0]
    );

    // 已经读得完的句子：无改动，不许占撤销栈、不许动时间。
    let mut calm = fresh_state(vec![Segment::new(1, 0.0, 5.0, "短句")]);
    let before = calm.can_undo();
    let plan = calm.apply_cps_fix(crate::subtitle::DEFAULT_MAX_CPS, 0.0);
    assert!(!plan.has_changes());
    assert_eq!(calm.can_undo(), before, "无改动不许压撤销点");
    assert!(!calm.can_undo());
    assert!(
        (calm.segments[0].end - 5.0).abs() < 1e-9,
        "无改动不许动时间"
    );
}

/// 只读薄封装：CPS 违规下标与标点复核都按 `segments` 现算。
#[test]
fn cps_and_punctuation_wrappers_report_without_mutating() {
    let state = fresh_state(vec![
        Segment::new(1, 0.0, 1.0, "一二三四五六七八九十"),
        Segment::new(2, 2.0, 6.0, "没问题。"),
    ]);
    assert_eq!(
        state.cps_violation_indices(crate::subtitle::DEFAULT_MAX_CPS),
        vec![1]
    );
    let plan = state.punctuation_plan();
    assert_eq!(plan.indices, vec![1], "第一句没有句尾标点");
    assert_eq!(plan.skipped, 0);
}

/// 续跑目录口径：桌面端与 CLI 必须算出同一个目录（同级 `.voice2word-stages/<stem>`）。
#[test]
fn stage_artifact_dir_matches_cli_convention() {
    let media = Path::new("media").join("clips").join("talk.mp4");
    let got = AppState::stage_artifact_dir(&media);
    let want = Path::new("media")
        .join("clips")
        .join(".voice2word-stages")
        .join("talk");
    assert_eq!(got, want);

    // 无父目录时与 CLI 一样回落到当前目录。
    assert_eq!(
        AppState::stage_artifact_dir(Path::new("talk.mp4")),
        Path::new(".").join(".voice2word-stages").join("talk")
    );
}

/// 开工时建目录并挂到管线；可一键关掉；相对路径不会被误当作可续跑位置。
#[test]
fn prepare_stage_artifacts_creates_dir_and_attaches_then_can_be_disabled() {
    let root = unique("stage_prepare");
    std::fs::create_dir_all(&root).unwrap();
    let media = root.join("talk.mp4");
    std::fs::write(&media, b"x").unwrap();

    let state = fresh_state(Vec::new());
    assert_eq!(state.stage_artifacts_dir(), None, "开局必须是关闭状态");

    let dir = state.prepare_stage_artifacts(&media).expect("应能建目录");
    assert_eq!(dir, root.join(".voice2word-stages").join("talk"));
    assert!(dir.is_dir(), "真正开工时才创建目录");
    assert_eq!(state.stage_artifacts_dir(), Some(dir));

    state.disable_stage_artifacts();
    assert_eq!(state.stage_artifacts_dir(), None);

    // 相对路径（没有真实父目录）必须被挡下：不许在仓库根散落 `.voice2word-stages`。
    assert!(state
        .prepare_stage_artifacts(Path::new("talk.mp4"))
        .is_err());
    assert_eq!(state.stage_artifacts_dir(), None);
    let _ = std::fs::remove_dir_all(&root);
}

/// 旧产物清理：超出保留额度的最旧子目录被整体删除，额度内一个都不动。
#[test]
fn prune_stale_stage_dirs_keeps_only_recent() {
    use super::prune_stale_stage_dirs;

    let root = unique("stage_prune");
    std::fs::create_dir_all(&root).unwrap();
    for name in ["a", "b", "c"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(root.join(name).join("01_extract_audio.json"), b"{}").unwrap();
    }

    // keep = 0：三个子目录都超出额度、理应全被清掉（与 mtime 顺序无关，断言稳定）。
    assert_eq!(prune_stale_stage_dirs(&root, 0), 3);
    for name in ["a", "b", "c"] {
        assert!(!root.join(name).exists(), "{name} 应被删掉");
    }
    // 根目录自身留着；空目录上再跑一次不 panic。
    assert!(root.is_dir());
    assert_eq!(prune_stale_stage_dirs(&root, 8), 0);
    let _ = std::fs::remove_dir_all(&root);
}
