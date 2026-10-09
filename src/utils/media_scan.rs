//! 拖放 / 选择路径 → 媒体文件清单的展开工具
//!
//! ## 为什么需要它
//!
//! 在此之前，把**文件夹**拖到窗口上等于什么都没发生：拖放处理只保留扩展名命中
//! 固定白名单的**文件**，目录项被直接丢掉。于是用户按最自然的操作拖进一个课程
//! 文件夹时，要么静默无反应，要么只收到一句笼统的「不支持的格式」，完全看不出
//! 文件夹本来就是被期望支持的输入。
//!
//! 本模块把「用户拖进来或选中的任意路径」统一展开成一份扁平、去重后的媒体文件
//! 清单，并附带一份结构化的「跳过了什么、为什么跳过」记录，让界面能给出可操作
//! 的提示（例如「扫了 12 个文件夹，跳过 1 个层级过深的目录」），而不是干瞪眼。
//!
//! ## 支持哪些格式由调用方决定
//!
//! 本模块**不**硬编码任何媒体类型：`exts` 由调用方传入。否则「支持哪些格式」
//! 会在拖放白名单和扫描器两处各存一份，迟早漂移成两个互相矛盾的答案。
//!
//! ## 顺序规则（测试依赖它）
//!
//! 每个目录内先处理**文件**、再递归**子目录**，各自按文件名（不区分大小写，
//! 同名时按完整路径兜底）升序排列。于是「先深搜文件、后深搜子目录」是一条固定
//! 规则，而不是取决于 `read_dir` 的返回次序——Windows 上 NTFS 的目录枚举顺序
//! 既非字母序也非创建序，不排序就没法写出稳定的断言。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 目录递归深度的默认上限。
///
/// 8 层足以覆盖「课程 / 章节 / 小节 / 素材」这类真实结构，又能挡住用户误拖
/// 整个盘符（如 `C:\`）时把整个磁盘遍历一遍的灾难。
pub const DEFAULT_MAX_DEPTH: usize = 8;

/// 为什么某个输入项没有被收进结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// 给定的文件扩展名不在支持列表里
    UnsupportedExtension,
    /// 目录层数超过 `max_depth`
    TooDeep,
    /// 读目录 / 读元数据失败（权限、盘符离线、路径不存在…）
    Unreadable,
    /// 符号链接 / 目录联接（junction）— 不跟随，避免环形链接无限递归
    Symlink,
}

impl SkipReason {
    /// 给界面用的一句话中文说明（可操作）
    ///
    /// 每条都写成「发生了什么 + 用户可以怎么做」，因为这类提示出现的场景正是
    /// 用户以为自己已经成功拖入了文件、却看不到任何结果的时候——只说「失败」
    /// 等于把排查工作丢回给用户。
    pub fn message(self) -> &'static str {
        match self {
            SkipReason::UnsupportedExtension => {
                "这个文件不是支持的媒体格式，请改用 mp4 / mp3 / wav / mkv 等常见音视频文件。"
            }
            SkipReason::TooDeep => {
                "文件夹层级太深，已超出扫描上限；请直接把更具体的子文件夹拖进来。"
            }
            SkipReason::Unreadable => "路径读取失败：可能不存在、没有访问权限，或所在磁盘未连接。",
            SkipReason::Symlink => "这是快捷方式或符号链接，已跳过以避免循环扫描。",
        }
    }
}

/// 一次扫描的结果。`files` 已按「路径小写」去重、保持**稳定**顺序（见下）。
///
/// 顺序规则：对每个目录，先输出其中命中的**文件**（按文件名升序），再按文件名
/// 升序递归其**子目录**。即整体是一次「文件优先的前序遍历」，同一份输入两次
/// 运行必然得到同一个 `files`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    /// 可处理的媒体文件（去重后的绝对/原始路径）
    pub files: Vec<std::path::PathBuf>,
    /// 实际进入过的目录数（供界面交代「扫了几个文件夹」）
    pub dirs_scanned: usize,
    /// 被跳过的项：`(原路径, 原因)`。只记录**输入项或其直接子项**级别的跳过，
    /// 不要为目录里的每一个非媒体文件都记一条（否则提示会被上千行噪声淹没）。
    pub skipped: Vec<(std::path::PathBuf, SkipReason)>,
}

/// 把用户给定的路径（文件或目录）展开成媒体文件列表。
///
/// - `exts` 由调用方传入（小写、不带点，例如 `&["mp4","mp3"]`），模块内**不硬编码**
///   媒体类型——否则「支持哪些格式」会在两处各存一份并迟早漂移。条目里带前导点
///   （`".mp4"`）或大小写混排都能被容忍，见 [`normalize_exts`]。
/// - `max_depth`：目录递归深度上限，0 表示只扫给定目录本身、不进子目录。
///
/// 函数不会写盘、不会跟随符号链接、不解析 `..`（`read_dir` 本身也不会产出
/// `..` 条目），对任何输入都只返回结果、不 panic。
pub fn collect_media_files(
    inputs: &[std::path::PathBuf],
    exts: &[&str],
    max_depth: usize,
) -> ScanOutcome {
    let mut state = ScanState::new(exts, max_depth);
    for input in inputs {
        state.push_input(input);
    }
    state.out
}

/// 扫描过程中累积的全部状态。
///
/// 单独抽成结构体而不是一路透传 `&mut` 参数：递归函数已经有 4 个可变状态
/// （结果、文件去重集、目录访问集、深度），继续加参数只会让签名难读且易错。
struct ScanState {
    /// 已归一化的扩展名（小写、无前导点）。
    exts: Vec<String>,
    max_depth: usize,
    /// 已收下的文件，键为「路径小写」——Windows 上 `A:\x.mp4` 与 `a:\X.MP4`
    /// 是同一个文件，按键去重避免同一集视频在清单里出现两次。
    seen_files: HashSet<String>,
    /// 已进入过的目录（规范化后的绝对路径），作为环形链接的兜底。
    visited_dirs: HashSet<PathBuf>,
    out: ScanOutcome,
}

impl ScanState {
    fn new(exts: &[&str], max_depth: usize) -> Self {
        Self {
            exts: normalize_exts(exts),
            max_depth,
            seen_files: HashSet::new(),
            visited_dirs: HashSet::new(),
            out: ScanOutcome::default(),
        }
    }

    /// 处理一个用户直接给出的路径（拖放/文件选择器的原始输入）。
    fn push_input(&mut self, input: &Path) {
        match classify(input) {
            // 路径不存在、盘符离线、权限不足：用户明确点名了它，必须告诉他
            Err(()) => self.skip(input, SkipReason::Unreadable),
            Ok(EntryKind::Symlink) => self.skip(input, SkipReason::Symlink),
            Ok(EntryKind::Other) => self.skip(input, SkipReason::Unreadable),
            Ok(EntryKind::File) => {
                if ext_matches(input, &self.exts) {
                    self.push_file(input);
                } else {
                    // 用户点名了一个非媒体文件：沉默会让他以为是程序坏了
                    self.skip(input, SkipReason::UnsupportedExtension);
                }
            }
            Ok(EntryKind::Dir) => self.scan_dir(input, 0),
        }
    }

    /// 递归扫描一个目录，`depth` 是它相对顶层输入的层级（顶层为 0）。
    fn scan_dir(&mut self, dir: &Path, depth: usize) {
        // 环形链接的兜底：符号链接已在 classify 里挡住，但 Windows 的目录联接
        // 在某些情况下 file_type() 报告的是目录而非链接，规范化路径是最后一道闸。
        if let Ok(canon) = std::fs::canonicalize(dir) {
            if !self.visited_dirs.insert(canon) {
                return;
            }
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => {
                // 无权限 / 中途被删除 / 盘符掉线：记为不可读而不是当作空目录，
                // 否则用户会看到「扫描完成，0 个文件」却不知道少了什么
                self.skip(dir, SkipReason::Unreadable);
                return;
            }
        };
        self.out.dirs_scanned += 1;

        let mut files: Vec<PathBuf> = Vec::new();
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            match classify(&path) {
                Ok(EntryKind::File) => {
                    if ext_matches(&path, &self.exts) {
                        files.push(path);
                    }
                    // 目录内的非媒体文件一律静默忽略：一个课程文件夹里可能有
                    // 讲义、封面、工程文件，逐个上报会把真正有用的提示淹掉
                }
                Ok(EntryKind::Dir) => subdirs.push(path),
                Ok(EntryKind::Symlink) => self.skip(&path, SkipReason::Symlink),
                // 元数据读不出来（读目录与读元数据之间文件被删/权限被收回）
                Ok(EntryKind::Other) | Err(()) => self.skip(&path, SkipReason::Unreadable),
            }
        }

        sort_by_name(&mut files);
        sort_by_name(&mut subdirs);

        for file in files {
            self.push_file(&file);
        }
        for sub in subdirs {
            // 用 `depth >= max_depth` 而不是 `depth + 1 > max_depth`：后者在
            // max_depth = usize::MAX 时会整型溢出 panic
            if depth >= self.max_depth {
                // 边界上的子目录只记一条，不再进去看它有没有媒体文件——
                // 「进都没进」是确定的，猜内容反而让结果依赖磁盘状态
                self.skip(&sub, SkipReason::TooDeep);
            } else {
                self.scan_dir(&sub, depth + 1);
            }
        }
    }

    /// 收下一个文件；按「路径小写」去重，首次出现的写法胜出。
    fn push_file(&mut self, path: &Path) {
        if self.seen_files.insert(dedup_key(path)) {
            self.out.files.push(path.to_path_buf());
        }
    }

    fn skip(&mut self, path: &Path, reason: SkipReason) {
        self.out.skipped.push((path.to_path_buf(), reason));
    }
}

/// 条目的粗分类。用 `symlink_metadata` 取得，因此**不会**跟随链接。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    /// 符号链接 / 目录联接
    Symlink,
    /// 既不是文件也不是目录（管道、设备…），按不可读处理
    Other,
}

/// 读一次元数据并分类；失败（不存在 / 无权限）返回 `Err(())`。
///
/// 这里刻意使用 `symlink_metadata` 而非 `metadata`：后者会跟随链接，遇到
/// 「链接指向自己」或两个目录互指时会让递归永远走不完。
fn classify(path: &Path) -> Result<EntryKind, ()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            Ok(if ft.is_symlink() {
                EntryKind::Symlink
            } else if ft.is_dir() {
                EntryKind::Dir
            } else if ft.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            })
        }
        Err(_) => Err(()),
    }
}

/// 把调用方给的扩展名归一化成「小写、无前导点」，便于与 `Path::extension()` 比较。
///
/// 容忍 `".mp4"` / `"MP4"` 这类写法：这些常量通常由 UI 层的过滤器顺手复用，
/// 在那里带上点号是很自然的写法，不该因此静默漏掉所有文件。
fn normalize_exts(exts: &[&str]) -> Vec<String> {
    exts.iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// 文件扩展名（不区分大小写）是否命中 `exts`。没有扩展名一律不命中。
fn ext_matches(path: &Path, exts: &[String]) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let ext = ext.to_ascii_lowercase();
            exts.contains(&ext)
        }
        None => false,
    }
}

/// 去重键：路径字符串的 ASCII 小写形式。
///
/// 用 ASCII 小写而不是 `to_lowercase()`：后者对非 ASCII 字符做 Unicode 折叠，
/// 可能把两个真实存在的不同文件名（如土耳其语 `İ` / `i`）误判成同一个；而
/// Windows 文件系统的路径比较本来就只忽略 ASCII 大小写。非 UTF-8 路径经
/// `to_string_lossy` 会丢信息，但那只影响极端路径，且最坏结果是少收一个文件。
fn dedup_key(path: &Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

/// 按文件名（不区分大小写）升序排列，同名时用完整路径兜底，保证全序。
fn sort_by_name(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| {
        let key = |p: &PathBuf| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
        };
        key(a).cmp(&key(b)).then_with(|| a.cmp(b))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独占一个带 tag 的临时目录，避免并行运行时互相踩。
    ///
    /// 目录名带进程 id：并发跑的多个 cargo test 进程不会撞车。调用方负责
    /// 开头 `remove_dir_all` 清残留、结尾再清一次。
    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("v2w_scan_{}_{tag}", std::process::id()))
    }

    /// 目录里的媒体文件会被收下；同目录的非媒体文件被静默忽略，不产生噪声。
    #[test]
    fn finds_media_and_ignores_other_files_in_dir() {
        let dir = temp_dir("basic");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.mp4"), b"x").unwrap();
        std::fs::write(dir.join("note.txt"), b"x").unwrap();

        let out = collect_media_files(std::slice::from_ref(&dir), &["mp4"], 8);

        assert_eq!(out.files, vec![dir.join("a.mp4")]);
        assert_eq!(out.dirs_scanned, 1);
        assert!(
            out.skipped.is_empty(),
            "目录内非媒体文件不该被记录：{:?}",
            out.skipped
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 嵌套子目录（深度 >= 2）里的媒体文件同样会被找到，目录数也照实统计。
    #[test]
    fn finds_media_in_nested_dirs() {
        let dir = temp_dir("nested");
        let _ = std::fs::remove_dir_all(&dir);
        let deep = dir.join("l1").join("l2");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("c.mp4"), b"x").unwrap();

        let out = collect_media_files(std::slice::from_ref(&dir), &["mp4"], 8);

        assert_eq!(out.files, vec![dir.join("l1").join("l2").join("c.mp4")]);
        assert_eq!(out.dirs_scanned, 3);
        assert!(
            out.skipped.is_empty(),
            "深度足够时不该有跳过项：{:?}",
            out.skipped
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `max_depth = 0` 时不进子目录（但目录本身的文件照收），
    /// 边界上的子目录只记一条 `TooDeep`。
    #[test]
    fn max_depth_zero_records_boundary_subdir_once() {
        let dir = temp_dir("depth0");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.mp4"), b"x").unwrap();
        std::fs::write(dir.join("sub").join("b.mp4"), b"x").unwrap();

        let out = collect_media_files(std::slice::from_ref(&dir), &["mp4"], 0);

        assert_eq!(out.files, vec![dir.join("a.mp4")]);
        assert_eq!(out.dirs_scanned, 1);
        assert_eq!(out.skipped, vec![(dir.join("sub"), SkipReason::TooDeep)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用户明确点名的非媒体文件要记为 `UnsupportedExtension`——他需要被明确告知，
    /// 而不是像目录内的杂项文件那样被静默忽略。
    #[test]
    fn explicit_non_media_file_is_reported() {
        let dir = temp_dir("explicit_txt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("note.txt");
        std::fs::write(&note, b"x").unwrap();

        let out = collect_media_files(std::slice::from_ref(&note), &["mp4"], 8);

        assert!(out.files.is_empty());
        assert_eq!(out.skipped, vec![(note, SkipReason::UnsupportedExtension)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不存在的路径记为 `Unreadable`，且不 panic。
    #[test]
    fn nonexistent_path_is_unreadable() {
        let missing = temp_dir("missing").join("nope.mp4");

        let out = collect_media_files(std::slice::from_ref(&missing), &["mp4"], 8);

        assert!(out.files.is_empty());
        assert_eq!(out.skipped, vec![(missing, SkipReason::Unreadable)]);
    }

    /// 重复路径（含大小写不同的同一路径）只保留一条，首次出现的写法胜出。
    #[test]
    fn duplicates_collapse_case_insensitively() {
        let dir = temp_dir("dedup");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.mp4");
        std::fs::write(&file, b"x").unwrap();
        let upper = PathBuf::from(file.to_string_lossy().to_ascii_uppercase());

        let out = collect_media_files(&[file.clone(), file.clone(), upper], &["mp4"], 8);

        assert_eq!(out.files, vec![file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 结果顺序确定：同目录内按文件名排序（a 在 b 前），且文件排在子目录之前。
    #[test]
    fn ordering_is_deterministic_files_before_subdirs() {
        let dir = temp_dir("order");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("zz")).unwrap();
        // 故意先建 b 再建 a：断言必须依赖排序规则，而不是创建顺序
        std::fs::write(dir.join("b.mp4"), b"x").unwrap();
        std::fs::write(dir.join("a.mp4"), b"x").unwrap();
        std::fs::write(dir.join("zz").join("c.mp4"), b"x").unwrap();

        let out = collect_media_files(std::slice::from_ref(&dir), &["mp4"], 8);

        assert_eq!(
            out.files,
            vec![
                dir.join("a.mp4"),
                dir.join("b.mp4"),
                dir.join("zz").join("c.mp4"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 直接传一个媒体文件（而不是文件夹）同样有效。
    #[test]
    fn explicit_media_file_is_collected() {
        let dir = temp_dir("explicit_media");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp3");
        std::fs::write(&file, b"x").unwrap();

        let out = collect_media_files(std::slice::from_ref(&file), &["mp3"], 8);

        assert_eq!(out.files, vec![file]);
        assert_eq!(out.dirs_scanned, 0);
        assert!(out.skipped.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 扩展名匹配不区分大小写，并且容忍 `exts` 条目带前导点。
    #[test]
    fn extension_matching_is_case_insensitive_and_tolerates_dot() {
        let dir = temp_dir("ext_case");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MOVIE.MP4"), b"x").unwrap();

        let out = collect_media_files(std::slice::from_ref(&dir), &[".Mp4"], 8);

        assert_eq!(out.files, vec![dir.join("MOVIE.MP4")]);
        assert!(out.skipped.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
