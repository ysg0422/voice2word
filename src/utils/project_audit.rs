//! 阶段产物（断点续跑缓存）的工程体检与清理。
//!
//! 转写管线会把每个阶段的中间结果落盘到媒体文件旁边：
//!
//! ```text
//! <素材目录>/.voice2word-stages/<文件 stem>/
//!     01_extract_audio.json
//!     03_transcribe.json
//!     07_write.json
//!     ...
//! ```
//!
//! 这些文件是「跑崩了不用从头来」的缓存，不是资产。它们会随着片子越跑越多
//! 而占满素材目录，而用户此前只能在文件管理器里手工找隐藏目录。本模块把
//! 「有哪些缓存、多大、还能不能用」做成可查询的数据，并提供两级清理：
//!
//! - [`clear_project`]：整目录删除，用于「这支片子的缓存不要了」；
//! - [`clear_stale`]：只删不可复用的单个产物，用于「留着能用的，别白跑」。
//!
//! 这里是**纯引擎**：不碰 UI、不读全局配置，只做文件系统扫描与删除，方便
//! 桌面端和命令行共用同一套口径。
//!
//! # 指纹判定：只能判「有没有」，不能判「对不对」
//!
//! 片段产物的信封形如
//! `{ "input": "...", "fingerprint": { ... }, "segments": [...] }`，其中
//! `fingerprint` 是落盘时的完整参数快照。要判断它是否仍能代表**当前**任务，
//! 必须重算一份指纹；而本模块没有模型、语言、预处理等配置上下文，重算不可行。
//!
//! 因此 [`StageArtifact::fingerprint_ok`] 只回答「信封里有没有指纹字段」：
//!
//! - `Some(true)`：有指纹，产物来自带指纹校验的管线版本，可复用（参数是否
//!   一致交给真正的续跑路径判断）；
//! - `Some(false)`：没有指纹（旧格式产物），续跑路径一定不会复用它，视为 stale；
//! - `None`：文件读不出/不是合法 JSON 信封，同样不可复用。
//!
//! `"fingerprint": null` 与「没有该字段」等价：两边都会被续跑路径解析成
//! `None`，同样不可复用。
//!
//! # 符号链接
//!
//! 扫描全程不跟随符号链接：产物文件的元数据用 [`std::fs::metadata`] 读取，
//! 但条目类型先由 `DirEntry::file_type()` 判定，链接目录不会被递归进入，
//! 链接文件也不会被列为产物。清理时遇到链接目录直接报错，绝不顺着链接删到
//! 别处去。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;

/// 阶段产物根目录名（与 `app::state` / `cli` 的口径一致）。
const STAGE_DIR_NAME: &str = ".voice2word-stages";

/// 递归扫描的深度上限。正常的素材目录远达不到这个深度；设上限只是为了在
/// 遇到异常文件系统（例如某些 Windows junction）时保证扫描一定能结束。
const MAX_SCAN_DEPTH: usize = 64;

/// 一份阶段产物文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageArtifact {
    /// 阶段名，取文件名主干：`03_transcribe.json` → `"03_transcribe"`。
    pub stage: String,
    /// 产物文件的完整路径。
    pub path: PathBuf,
    /// 文件字节数（元数据读不到时为 0）。
    pub bytes: u64,
    /// 修改时间；文件系统不提供时为 `None`。
    pub modified: Option<SystemTime>,
    /// 指纹状态：`Some(true)` 有指纹；`Some(false)` 旧产物；`None` 读不出信封。
    ///
    /// 这里只判断「有没有指纹」，不判断指纹是否与当前任务一致，原因见模块文档。
    pub fingerprint_ok: Option<bool>,
}

/// 一个阶段产物目录（通常对应一支媒体文件）的体检结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAudit {
    /// 阶段产物目录本身。
    pub dir: PathBuf,
    /// 目录内的产物文件，按阶段名排序。
    pub artifacts: Vec<StageArtifact>,
    /// 所有产物字节数之和。
    pub total_bytes: u64,
}

/// 扫描某个媒体文件对应的阶段产物目录。
///
/// 目录口径与 GUI / CLI 完全一致：`<媒体所在目录>/.voice2word-stages/<文件 stem>`。
/// 目录不存在、不是目录、或是符号链接时返回空列表（`artifacts` 为空、
/// `total_bytes == 0`），不报错——体检是只读操作，不能因为缓存缺失就打断调用方。
///
/// 返回值里的 [`ProjectAudit::dir`] 仍然是**计算出的目录路径**，即使它当前
/// 并不存在；这样界面可以直接展示「缓存目录：…（空）」，也方便随后清理。
pub fn audit_media(media: &Path) -> ProjectAudit {
    audit_dir(media_artifact_dir(media))
}

/// 扫描某目录下的**全部**阶段产物：`<root>/**/.voice2word-stages/*`。
///
/// 每个 `.voice2word-stages` 下的子目录对应一支媒体（一个 [`ProjectAudit`]）。
/// 结果按 [`ProjectAudit::dir`] 排序，保证同一份磁盘状态每次得到相同顺序。
/// 根目录不存在或读不到时返回空列表，不报错。
///
/// 扫描不跟随符号链接；遇到 `.voice2word-stages` 后不再向下递归（产物目录
/// 内部只会有文件，继续递归既没有意义也容易踩到异常目录结构）。
pub fn audit_root(root: &Path) -> Vec<ProjectAudit> {
    let mut audits = Vec::new();
    collect_stage_roots(root, 0, &mut audits);
    audits.sort_by(|left, right| left.dir.cmp(&right.dir));
    audits
}

/// 删除整个阶段产物目录，返回释放的字节数。
///
/// 释放量在删除**之前**取自 [`ProjectAudit::total_bytes`]（扫描时的快照），
/// 因为 `remove_dir_all` 成功返回后文件已经不存在，无法再统计。目录已经不存在
/// 时返回 `Ok(0)`，让「一键清理」可以重复点击。
///
/// 拒绝清理符号链接目录：`remove_dir_all` 的错误信息在 Windows 上不够明确，
/// 这里提前给出可读原因，也避免任何实现差异导致顺着链接删到用户别处。
pub fn clear_project(audit: &ProjectAudit) -> anyhow::Result<u64> {
    let meta = match std::fs::symlink_metadata(&audit.dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取阶段产物目录失败: {}", audit.dir.display()));
        }
    };
    if meta.file_type().is_symlink() {
        anyhow::bail!("阶段产物目录是符号链接，拒绝清理: {}", audit.dir.display());
    }
    if !meta.is_dir() {
        // 路径已经被换成文件：不是本模块该删除的东西，安静地当作「没有目录」。
        return Ok(0);
    }

    let freed = audit.total_bytes;
    std::fs::remove_dir_all(&audit.dir)
        .with_context(|| format!("删除阶段产物目录失败: {}", audit.dir.display()))?;
    Ok(freed)
}

/// 只删除「指纹不匹配/读不出」的产物文件，保留可复用的，返回删除个数。
///
/// 判定标准与 [`StageArtifact::fingerprint_ok`] 一致：只保留 `Some(true)`；
/// `Some(false)`（旧格式，续跑路径不会复用）与 `None`（坏 JSON）都删除。
/// 只删单个文件，不删除目录，也不碰目录里的非 `.json` 文件；单个文件删除失败
/// 时跳过并继续，返回值只统计真正删掉的数量。
pub fn clear_stale(audit: &ProjectAudit) -> usize {
    let mut removed = 0usize;
    for artifact in &audit.artifacts {
        if artifact.fingerprint_ok == Some(true) {
            continue;
        }
        // 扫描阶段已排除链接；这里再查一次，防止审计与清理之间路径被替换。
        let is_symlink = std::fs::symlink_metadata(&artifact.path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if is_symlink {
            continue;
        }
        if std::fs::remove_file(&artifact.path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 人类可读的字节数：`512 B` / `340 KB` / `1.2 MB` / `2.5 GB`。
///
/// 口径与 [`crate::utils::model_download::human_size`] 保持一致：
///
/// - 1 KB = 1024 B（与磁盘容量、任务管理器一致）；
/// - 不足 1 KB 显示整数字节，不带小数；
/// - 其余单位保留一位小数，并抹掉末尾的 `.0`（`340.0 KB` → `340 KB`）；
/// - 超过 GB 仍以 GB 显示。
pub fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let value = bytes as f64;
    let (scaled, unit) = if value >= GB {
        (value / GB, "GB")
    } else if value >= MB {
        (value / MB, "MB")
    } else if value >= KB {
        (value / KB, "KB")
    } else {
        return format!("{bytes} B");
    };
    let text = format!("{scaled:.1}");
    let text = text.strip_suffix(".0").unwrap_or(text.as_str());
    format!("{text} {unit}")
}

/// `媒体文件 → 阶段产物目录`：`<文件所在目录>/.voice2word-stages/<文件 stem>`。
///
/// 与 `app::state::stage_artifact_dir` / CLI 的 `resolve_artifact_root` 逐字一致：
/// 取不到 stem 时回落 `"output"`，没有父目录时回落当前目录。
fn media_artifact_dir(media: &Path) -> PathBuf {
    let stem = media
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let base = match media.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    base.join(STAGE_DIR_NAME).join(stem)
}

/// 扫描一个具体的产物目录；目录缺失/不可读时返回空审计。
fn audit_dir(dir: PathBuf) -> ProjectAudit {
    let mut audit = ProjectAudit {
        dir,
        artifacts: Vec::new(),
        total_bytes: 0,
    };

    // `metadata` 跟随链接，所以先用 `symlink_metadata` 拒绝链接目录，
    // 保证后续 `read_dir` 不会读到链接目标。
    if is_symlink(&audit.dir) {
        return audit;
    }
    let Ok(meta) = std::fs::metadata(&audit.dir) else {
        return audit;
    };
    if !meta.is_dir() {
        return audit;
    }
    let Ok(entries) = std::fs::read_dir(&audit.dir) else {
        return audit;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // `DirEntry::file_type()` 不跟随链接：链接文件的 is_file() 为 false。
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        // 阶段产物只有 JSON；目录里偶尔会残留 `.part` 临时文件或用户放的说明文件，
        // 它们不是阶段产物，既不展示也不由 `clear_stale` 删除。
        let is_json = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        if !is_json {
            continue;
        }
        let Some(stage) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let bytes = meta.len();
        audit.artifacts.push(StageArtifact {
            stage,
            path: path.clone(),
            bytes,
            modified: meta.modified().ok(),
            fingerprint_ok: read_fingerprint_state(&path),
        });
        audit.total_bytes = audit.total_bytes.saturating_add(bytes);
    }

    audit.artifacts.sort_by(|left, right| {
        left.stage
            .cmp(&right.stage)
            .then_with(|| left.path.cmp(&right.path))
    });
    audit
}

/// 读取产物信封并判断指纹状态。
///
/// - 文件读不出 / JSON 解析失败 / 顶层不是对象 → `None`；
/// - 对象里没有 `fingerprint`，或值为 `null` → `Some(false)`（旧产物，不可复用）；
/// - 其他任何非空值 → `Some(true)`。
fn read_fingerprint_state(path: &Path) -> Option<bool> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let object = value.as_object()?;
    // `01_extract_audio` / `02_*` 这类产物**没有指纹字段**，管线只按归属（input）
    // 决定能否复用。若一律按「无指纹 = 过期」处理，会把**可复用**的缓存删掉，
    // 下次续跑白重跑一次音频抽取。
    let stage = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    if stage.starts_with("01_") || stage.starts_with("02_") {
        return Some(object.contains_key("input"));
    }
    Some(match object.get("fingerprint") {
        Some(fingerprint) => !fingerprint.is_null(),
        None => false,
    })
}

/// 递归寻找 `<dir>/**/.voice2word-stages`。
fn collect_stage_roots(dir: &Path, depth: usize, audits: &mut Vec<ProjectAudit>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    if is_symlink(dir) {
        return;
    }
    let Ok(meta) = std::fs::metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }

    if dir.file_name() == Some(OsStr::new(STAGE_DIR_NAME)) {
        scan_stage_root(dir, audits);
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // 链接目录的 `is_dir()` 为 false，天然不会被递归。
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_stage_roots(&entry.path(), depth + 1, audits);
        }
    }
}

/// 一个 `.voice2word-stages` 下的每个子目录都是一支媒体的产物目录。
fn scan_stage_root(root: &Path, audits: &mut Vec<ProjectAudit>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            audits.push(audit_dir(entry.path()));
        }
    }
}

/// 路径本身是否是符号链接（不跟随链接）。
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 临时目录守卫：测试结束（包括断言 panic）时自动删除。
    struct TempTree {
        path: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "v2w_project_audit_{}_{}_{}",
                std::process::id(),
                id,
                tag
            ));
            // 上一次异常退出可能留下同名目录，先清掉再建。
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn stage_dir(&self, stem: &str) -> PathBuf {
            self.path.join(STAGE_DIR_NAME).join(stem)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建产物目录");
        }
        fs::write(path, body).expect("写产物文件");
    }

    /// 模拟管线信封：`input` 必有，`fingerprint` 可选。
    fn envelope(with_fingerprint: bool) -> String {
        if with_fingerprint {
            r#"{"input":"C:\\media\\talk.mp4","fingerprint":{"model":"base","language":"zh"},"segments":[{"start":0.0,"end":1.0,"text":"hi"}]}"#.to_string()
        } else {
            r#"{"input":"C:\\media\\talk.mp4","segments":[{"start":0.0,"end":1.0,"text":"hi"}]}"#
                .to_string()
        }
    }

    #[test]
    fn missing_stage_dir_returns_empty_audit() {
        let tree = TempTree::new("missing");
        let media = tree.path().join("talk.mp4");
        // 媒体本身存在，只是没有阶段产物目录。
        write_file(&media, "not really a video");

        let audit = audit_media(&media);

        assert!(audit.artifacts.is_empty());
        assert_eq!(audit.total_bytes, 0);
        assert_eq!(
            audit.dir,
            tree.path().join(STAGE_DIR_NAME).join("talk"),
            "即使目录不存在，也要返回口径一致的路径"
        );
    }

    #[test]
    fn lists_stage_names_and_sizes() {
        let tree = TempTree::new("list");
        let stage_dir = tree.stage_dir("talk");
        let transcribe = stage_dir.join("03_transcribe.json");
        let write = stage_dir.join("07_write.json");
        let transcript_body = envelope(true);
        let write_body =
            r#"{"input":"C:\\media\\talk.mp4","fingerprint":{"model":"base"},"result":"ok"}"#;
        write_file(&transcribe, &transcript_body);
        write_file(&write, write_body);
        // 非 JSON 文件不是阶段产物，不应出现在审计里。
        write_file(&stage_dir.join("notes.txt"), "hello");

        let media = tree.path().join("talk.mp4");
        let audit = audit_media(&media);

        let stages: Vec<&str> = audit
            .artifacts
            .iter()
            .map(|artifact| artifact.stage.as_str())
            .collect();
        assert_eq!(stages, ["03_transcribe", "07_write"]);
        assert_eq!(audit.artifacts[0].bytes, transcript_body.len() as u64);
        assert_eq!(audit.artifacts[1].bytes, write_body.len() as u64);
        assert_eq!(
            audit.total_bytes,
            transcript_body.len() as u64 + write_body.len() as u64
        );
        assert!(
            audit
                .artifacts
                .iter()
                .all(|artifact| artifact.modified.is_some()),
            "普通文件应能读到修改时间"
        );
    }

    #[test]
    fn fingerprints_are_classified_by_presence() {
        let tree = TempTree::new("fingerprint");
        let stage_dir = tree.stage_dir("talk");
        write_file(&stage_dir.join("03_transcribe.json"), &envelope(true));
        write_file(&stage_dir.join("05_punctuate.json"), &envelope(false));
        write_file(
            &stage_dir.join("06_polish.json"),
            r#"{"input":"C:\\media\\talk.mp4","fingerprint":null}"#,
        );
        write_file(&stage_dir.join("07_write.json"), "{ not json");

        let audit = audit_media(&tree.path().join("talk.mp4"));
        let state = |stage: &str| {
            audit
                .artifacts
                .iter()
                .find(|artifact| artifact.stage == stage)
                .unwrap_or_else(|| panic!("缺少阶段产物: {stage}"))
                .fingerprint_ok
        };

        assert_eq!(state("03_transcribe"), Some(true));
        assert_eq!(state("05_punctuate"), Some(false));
        // null 与「没有字段」在续跑路径上等价，都不可复用。
        assert_eq!(state("06_polish"), Some(false));
        assert_eq!(state("07_write"), None);
    }

    #[test]
    fn clear_stale_removes_only_unusable_artifacts() {
        let tree = TempTree::new("clear_stale");
        let stage_dir = tree.stage_dir("talk");
        let fresh = stage_dir.join("03_transcribe.json");
        let old = stage_dir.join("05_punctuate.json");
        let broken = stage_dir.join("07_write.json");
        let unrelated = stage_dir.join("keep.part");
        write_file(&fresh, &envelope(true));
        write_file(&old, &envelope(false));
        write_file(&broken, "{ not json");
        write_file(&unrelated, "partial");

        let audit = audit_media(&tree.path().join("talk.mp4"));
        let removed = clear_stale(&audit);

        assert_eq!(removed, 2, "只应删除旧产物与坏 JSON");
        assert!(fresh.is_file(), "有指纹的产物必须保留");
        assert!(!old.exists(), "无指纹的旧产物应删除");
        assert!(!broken.exists(), "读不出信封的产物应删除");
        assert!(unrelated.is_file(), "非阶段产物文件不能被误删");
        assert!(stage_dir.is_dir(), "clear_stale 只删文件，不删目录");
    }

    #[test]
    fn clear_project_removes_directory_and_reports_freed_bytes() {
        let tree = TempTree::new("clear_project");
        let stage_dir = tree.stage_dir("talk");
        write_file(&stage_dir.join("03_transcribe.json"), &envelope(true));
        write_file(&stage_dir.join("07_write.json"), &envelope(false));

        let audit = audit_media(&tree.path().join("talk.mp4"));
        assert!(audit.total_bytes > 0);
        let expected = audit.total_bytes;

        let freed = clear_project(&audit).expect("清理阶段产物目录");

        assert_eq!(freed, expected);
        assert!(freed > 0);
        assert!(!audit.dir.exists(), "清理后整个产物目录应消失");
        // 第二次清理应当幂等，而不是报错。
        assert_eq!(clear_project(&audit).expect("重复清理"), 0);
    }

    #[test]
    fn audit_root_finds_all_nested_projects() {
        let tree = TempTree::new("root");
        let first = tree.path().join(STAGE_DIR_NAME).join("talk");
        let second = tree.path().join("clips").join(STAGE_DIR_NAME).join("demo");
        write_file(&first.join("03_transcribe.json"), &envelope(true));
        write_file(&second.join("05_punctuate.json"), &envelope(false));
        // 与阶段产物无关的普通目录不应产生审计。
        write_file(&tree.path().join("docs").join("readme.md"), "hi");

        let audits = audit_root(tree.path());

        assert_eq!(audits.len(), 2);
        assert!(audits.iter().all(|audit| audit.total_bytes > 0));
        assert!(
            audits.windows(2).all(|pair| pair[0].dir < pair[1].dir),
            "结果必须按目录排序，保证稳定"
        );
        assert!(audits.iter().any(|audit| audit.dir == first));
        assert!(audits.iter().any(|audit| audit.dir == second));
    }

    #[test]
    fn audit_root_missing_or_empty_root_is_quiet() {
        let tree = TempTree::new("empty_root");
        let missing = tree.path().join("does-not-exist");

        assert!(audit_root(&missing).is_empty());
        assert!(audit_root(tree.path()).is_empty());
    }

    #[test]
    fn human_bytes_boundaries() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(340 * 1024), "340 KB");
        assert_eq!(human_bytes(1024 * 1024 * 3 / 2), "1.5 MB");
        assert_eq!(human_bytes(1024 * 1024 + 1024 * 300), "1.3 MB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1 GB");
        assert_eq!(human_bytes(1024 * 1024 * 1024 * 5 / 2), "2.5 GB");
    }
}
