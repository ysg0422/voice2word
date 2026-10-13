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

/// 拖入项按扩展名分成的三类。
///
/// 这是**纯扩展名判定**，不碰文件系统：真正读盘失败的路径交给各自的导入内核报错
/// （媒体走 ffmpeg 探测、字幕走 `read_subtitle_file`），比在这里静默丢掉一个
/// 路径要有用得多——用户点名了一个文件却什么反馈都没有，是最难排查的那种体验。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DroppedKind {
    /// 命中媒体扩展名 —— 走转写队列
    Media,
    /// 命中字幕扩展名 —— 走「导入字幕」链路
    Subtitle,
    /// 两者都不命中 —— 既有的「非媒体文件」提示
    Other,
}

/// 判定一个用户**直接点名**的路径属于哪一类（媒体 / 字幕 / 其它）。
///
/// 两个清单都由调用方传入，本模块不硬编码任何格式（理由见模块头）。扩展名判定
/// 不区分大小写、并容忍清单里带前导点（`.SRT` 同样命中）。两个清单同时命中时
/// **媒体优先**：媒体清单是「进转写队列」的既有契约，字幕只是新增的旁路，不能让
/// 新旁路抢走本来会排队转写的文件。
///
/// 只按扩展名判定，**不读盘**：路径不存在、或是目录，都不改变这里的结论——真正的
/// 读盘失败由各自的导入内核给出明确报错。
pub fn classify_dropped_path(
    path: &Path,
    media_exts: &[&str],
    subtitle_exts: &[&str],
) -> DroppedKind {
    let media = normalize_exts(media_exts);
    let subtitle = normalize_exts(subtitle_exts);
    if ext_matches(path, &media) {
        DroppedKind::Media
    } else if ext_matches(path, &subtitle) {
        DroppedKind::Subtitle
    } else {
        DroppedKind::Other
    }
}

/// 从用户拖入 / 选中的路径里挑出**字幕文件**。
///
/// 与 [`collect_media_files`] 最大的区别是：**不递归目录**。一个课程文件夹里混着
/// JSON 元数据、歌词、笔记是常态，把目录里的 `.json` 认成字幕自动导入，会直接把
/// 用户当前的工程顶掉——这种破坏性动作必须由用户点名，不能靠扫目录猜。目录里是否
/// 藏着字幕，交给用户打开目录、把单个文件拖进来。
///
/// 也不检查路径是否存在（判定纯按扩展名），读盘失败由导入内核给出明确报错。结果按
/// **输入顺序**去重：同一路径大小写不同只保留首次出现的写法，保持稳定顺序便于测试。
pub fn collect_subtitle_files(inputs: &[PathBuf], exts: &[&str]) -> Vec<PathBuf> {
    let norm = normalize_exts(exts);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for input in inputs {
        if !ext_matches(input, &norm) {
            continue;
        }
        if seen.insert(dedup_key(input)) {
            out.push(input.clone());
        }
    }
    out
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

    // ── 拖拽分类（P1-A11）：字幕文件不再被静默忽略 ──────────────────────

    /// 媒体 / 字幕 / 其它三类由扩展名决定，且大小写不敏感、清单里带点也认。
    #[test]
    fn dropped_kind_is_extension_based_and_case_insensitive() {
        let media = ["mp4", "mp3"];
        let subs = [".SRT", "vtt", "json"];

        assert_eq!(
            classify_dropped_path(Path::new(r"A:\a\clip.MP4"), &media, &subs),
            DroppedKind::Media
        );
        assert_eq!(
            classify_dropped_path(Path::new("b.srt"), &media, &subs),
            DroppedKind::Subtitle
        );
        assert_eq!(
            classify_dropped_path(Path::new("c.VTT"), &media, &subs),
            DroppedKind::Subtitle
        );
        assert_eq!(
            classify_dropped_path(Path::new("notes.txt"), &media, &subs),
            DroppedKind::Other
        );
        // 没有扩展名一律不算命中
        assert_eq!(
            classify_dropped_path(Path::new("noext"), &media, &subs),
            DroppedKind::Other
        );
    }

    /// 两个清单同时命中时媒体优先：新加的字幕旁路不许抢走本该进转写队列的文件。
    #[test]
    fn media_list_wins_when_both_lists_match() {
        assert_eq!(
            classify_dropped_path(Path::new("x.mp4"), &["mp4"], &["mp4"]),
            DroppedKind::Media
        );
    }

    /// 只收「点名的那个路径」，媒体与非字幕文件都留在外面，且保持输入顺序。
    #[test]
    fn collect_subtitle_files_keeps_only_named_subtitles_in_input_order() {
        let inputs = vec![
            PathBuf::from("b.srt"),
            PathBuf::from("clip.mp4"),
            PathBuf::from("a.SSA"),
            PathBuf::from("notes.txt"),
            PathBuf::from("sub"),
            PathBuf::from("c.TTML"),
        ];

        let got = collect_subtitle_files(&inputs, &["srt", "ssa", "ttml", "json"]);

        assert_eq!(
            got,
            vec![
                PathBuf::from("b.srt"),
                PathBuf::from("a.SSA"),
                PathBuf::from("c.TTML"),
            ]
        );
    }

    /// 目录**不递归**：即使里面躺着字幕，拖目录进来也不会把它当成一次字幕导入。
    #[test]
    fn collect_subtitle_files_does_not_recurse_directories() {
        let dir = temp_dir("subtitle_no_recurse");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("inside.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nhi\n",
        )
        .unwrap();

        let got = collect_subtitle_files(std::slice::from_ref(&dir), &["srt"]);

        assert!(got.is_empty(), "拖目录不该扫出里面的字幕：{got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一路径（含大小写不同的写法）只留一条，首次出现的写法胜出。
    #[test]
    fn collect_subtitle_files_dedupes_case_insensitively() {
        let inputs = vec![
            PathBuf::from("a.srt"),
            PathBuf::from("A.SRT"),
            PathBuf::from("a.srt"),
        ];

        let got = collect_subtitle_files(&inputs, &["srt"]);

        assert_eq!(got, vec![PathBuf::from("a.srt")]);
    }

    /// 用户点名了一个不存在 / 不可读的字幕路径：收集阶段照样点名它，读盘失败由
    /// 导入内核报错——在这里悄悄丢掉会让用户以为程序没收到他的拖拽。
    #[test]
    fn collect_subtitle_files_keeps_missing_paths_for_the_import_kernel() {
        let missing = temp_dir("subtitle_missing").join("nope.srt");

        let got = collect_subtitle_files(std::slice::from_ref(&missing), &["srt"]);

        assert_eq!(got, vec![missing]);
    }
}

/// 超大 / 超长素材的提前提醒阈值（不是安全漏洞，是「别把用户拖死」）。
///
/// 抽帧缓存、波形、切块临时目录都会随片长线性变大：一个 8 小时 4K 素材能让磁盘
/// 和内存同时吃紧，用户看到的却是「界面没反应」。所以**先算一遍账再问一句**，
/// 而不是等它跑半小时才发现。阈值取「正常课程/会议素材的上限」再留一倍余量。
pub const MEDIA_SIZE_WARN_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// 时长的提醒线（秒）：6 小时。
pub const MEDIA_DURATION_WARN_SECS: f64 = 6.0 * 3600.0;
/// 硬上限：超过就拒绝开工（磁盘/内存风险已经不是「慢一点」的量级了）。
pub const MEDIA_SIZE_REFUSE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// 时长的硬上限（秒）：24 小时。
pub const MEDIA_DURATION_REFUSE_SECS: f64 = 24.0 * 3600.0;

/// 素材载入前的体检结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaLoadAdvice {
    /// 正常大小，直接开工。
    Ok,
    /// 偏大，问一句再继续（文案直接给用户看）。
    Warn(String),
    /// 超出硬上限，拒绝开工（文案直接给用户看）。
    Refuse(String),
}

/// 按文件大小与时长给出「提醒 / 拒绝」的结论。
///
/// `duration_secs` 传 0.0 表示还没探到时长（例如刚导入还没跑 ffprobe）：这时
/// 只按体积判断，绝不因为「时长未知」就当正常放行到拒绝那档。
///
/// `size_bytes` 传 0 同样只能理解成「不知道」（或确实空文件），而不是「确认很小」：
/// 拖放体检的调用点就是在 `metadata` 失败时这么传的（网络盘掉线 / 权限被临时收回 /
/// 文件刚被删）。这是**有意的降级**——体检只负责提前提醒代价，真正的读盘失败会由
/// ffmpeg 探测那条路给出明确报错；反过来「拿不到大小就一律拦下」会把共享盘的瞬时
/// 抖动变成用户眼里的「拖进去没反应」。要更保守也应在**调用点**把「读不到元数据」
/// 变成一条可见提示，而不是在这里编一个假体积：本函数只拿到一个数字，分不出
/// 「0 字节」和「未知」。
pub fn assess_media_load(size_bytes: u64, duration_secs: f64) -> MediaLoadAdvice {
    let hours = duration_secs / 3600.0;
    let gib = size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let has_duration = duration_secs > 0.0;

    if size_bytes >= MEDIA_SIZE_REFUSE_BYTES
        || (has_duration && duration_secs >= MEDIA_DURATION_REFUSE_SECS)
    {
        return MediaLoadAdvice::Refuse(format!(
            "这个素材约 {gib:.1} GB{}，超出了本工具的处理上限（{:.0} GB / {:.0} 小时）。请先切片再导入。",
            if has_duration {
                format!("、{hours:.1} 小时")
            } else {
                String::new()
            },
            MEDIA_SIZE_REFUSE_BYTES as f64 / (1024.0 * 1024.0 * 1024.0),
            MEDIA_DURATION_REFUSE_SECS / 3600.0,
        ));
    }

    if size_bytes >= MEDIA_SIZE_WARN_BYTES
        || (has_duration && duration_secs >= MEDIA_DURATION_WARN_SECS)
    {
        return MediaLoadAdvice::Warn(format!(
            "这个素材约 {gib:.1} GB{}，转写与预览会明显变慢，临时文件也会占不少磁盘。可以继续，也可以先切片。",
            if has_duration {
                format!("、{hours:.1} 小时")
            } else {
                String::new()
            },
        ));
    }

    MediaLoadAdvice::Ok
}

#[cfg(test)]
mod media_load_advice_tests {
    use super::{
        assess_media_load, MediaLoadAdvice, MEDIA_DURATION_REFUSE_SECS, MEDIA_SIZE_REFUSE_BYTES,
        MEDIA_SIZE_WARN_BYTES,
    };

    /// 正常素材必须走 Ok：阈值只用来兜极端情况，不能把普通长课/会议挡下来。
    #[test]
    fn ordinary_length_media_passes_without_questions() {
        // 3 小时 2 GB：典型的网课/会议录屏
        assert_eq!(
            assess_media_load(2 * 1024 * 1024 * 1024, 3.0 * 3600.0),
            MediaLoadAdvice::Ok
        );
        assert_eq!(assess_media_load(0, 0.0), MediaLoadAdvice::Ok);
    }

    /// 提醒线是「含等号」的：正好 8 GiB / 正好 6 小时都要问一句。
    #[test]
    fn warn_thresholds_are_inclusive() {
        assert!(matches!(
            assess_media_load(MEDIA_SIZE_WARN_BYTES, 0.0),
            MediaLoadAdvice::Warn(_)
        ));
        assert!(matches!(
            assess_media_load(1024, 6.0 * 3600.0),
            MediaLoadAdvice::Warn(_)
        ));
        assert!(matches!(
            assess_media_load(MEDIA_SIZE_WARN_BYTES - 1, 6.0 * 3600.0 - 1.0),
            MediaLoadAdvice::Ok
        ));
    }

    /// 时长未知（0.0）时只按体积判断，不能因为「不知道多长」就放过超大文件。
    #[test]
    fn unknown_duration_still_uses_size() {
        assert!(matches!(
            assess_media_load(MEDIA_SIZE_WARN_BYTES, 0.0),
            MediaLoadAdvice::Warn(_)
        ));
        assert!(matches!(
            assess_media_load(64 * 1024 * 1024 * 1024, 0.0),
            MediaLoadAdvice::Refuse(_)
        ));
    }

    /// 硬上限线同样是「含等号」的：正好 24 小时 / 正好 64 GiB 都拒绝，
    /// 差一点就落回提醒档——边界两侧各钉一个，免得以后有人把 `>=` 改成 `>`。
    #[test]
    fn refuse_thresholds_are_inclusive_and_one_below_stay_warn() {
        assert!(matches!(
            assess_media_load(1024, MEDIA_DURATION_REFUSE_SECS),
            MediaLoadAdvice::Refuse(_)
        ));
        assert!(matches!(
            assess_media_load(1024, MEDIA_DURATION_REFUSE_SECS - 1.0),
            MediaLoadAdvice::Warn(_)
        ));
        assert!(matches!(
            assess_media_load(MEDIA_SIZE_REFUSE_BYTES - 1, 0.0),
            MediaLoadAdvice::Warn(_)
        ));
        assert!(matches!(
            assess_media_load(MEDIA_SIZE_REFUSE_BYTES, 0.0),
            MediaLoadAdvice::Refuse(_)
        ));
    }

    /// 时长不是有限正数时不许参与判定（0 = 还没探到、NaN / 负数 = 脏数据）：
    /// 全部退化成「只看体积」，绝不能让脏时长把小文件误判成超限。
    #[test]
    fn non_positive_or_nan_duration_falls_back_to_size_only() {
        assert!(matches!(
            assess_media_load(1024, f64::NAN),
            MediaLoadAdvice::Ok
        ));
        assert!(matches!(assess_media_load(1024, -1.0), MediaLoadAdvice::Ok));
        // 正无穷是「明确的超长」，照拒绝走
        assert!(matches!(
            assess_media_load(1024, f64::INFINITY),
            MediaLoadAdvice::Refuse(_)
        ));
        // 体积已经超限时，脏时长也不能把它洗成 Ok
        assert!(matches!(
            assess_media_load(MEDIA_SIZE_REFUSE_BYTES, f64::NAN),
            MediaLoadAdvice::Refuse(_)
        ));
    }

    /// 硬上限以上直接拒绝，且文案里带得出实际体量（用户得知道为什么被挡）。
    #[test]
    fn extreme_media_is_refused_with_numbers() {
        match assess_media_load(80 * 1024 * 1024 * 1024, 30.0 * 3600.0) {
            MediaLoadAdvice::Refuse(msg) => {
                assert!(msg.contains("80.0 GB"), "文案要给出实际体量：{msg}");
                assert!(msg.contains("30.0 小时"), "文案要给出实际时长：{msg}");
                assert!(msg.contains("切片"), "要给出可行做法：{msg}");
            }
            other => panic!("超限素材必须拒绝，实际：{other:?}"),
        }
    }
}
