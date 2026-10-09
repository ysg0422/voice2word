//! 批量队列的磁盘持久化。
//!
//! # 为什么需要这个模块
//!
//! 批量转写队列此前只活在内存里（`app::state::AppState::batch_queue`）。用户挑 30
//! 个讲座视频排好队、跑了几条、然后关掉程序（或进程被强杀），整条队列连同「哪几条
//! 已经跑完」一起消失；下次只能重新选文件，进度也全丢了。这个模块把队列落成一个小
//! JSON 文件，启动时再读回来，把「重新挑文件 + 重新回忆跑到哪了」这件事消掉。
//!
//! # 为什么不塞进 SQLite
//!
//! 任务库（`storage/db.rs`）记录的是**已产出的成果**，而队列是**易失的界面状态**：
//! 同一条文件可能今天排队、明天删掉、后天再排。若两者混在一张表里，「删一条库记录」
//! 和「从队列里移除一项」就会互相牵动——用户删掉历史记录，队列里那条也跟着没了；
//! 用户清空队列，历史成果又跟着被删。把队列单独放一个小文件，是最省心也最不容易
//! 出意外的边界。
//!
//! # 失败模式与对策
//!
//! - **强杀 / 断电把文件截断成 0 字节**：写盘一律走「先写 `.tmp` 再 rename」，见
//!   [`save_queue`]；直接 `fs::write` 会先截断原文件，正是本功能最该兜住的场景。
//! - **文件损坏导致程序起不来**：队列是可选的辅助状态，[`load_queue`] 对**任何**
//!   异常都返回空 `Vec`，绝不返回 `Err`——一个坏文件没有资格阻断启动。
//! - **磁盘格式随内存结构漂移**：只存 [`StoredQueueItem`] 这个稳定 DTO，状态用
//!   字符串而不是枚举，新增状态时老文件仍可解析。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 队列持久化用的单条记录（只存必要的字段）。
///
/// 为什么不是直接序列化 `QueueItem`：那个类型在 `app` 层，`utils` 不该反向依赖它；
/// 而且它的状态枚举一旦增删字段，磁盘上的老文件就会解析失败。用一个**稳定**的
/// DTO + 显式的转换，把「磁盘格式」与「内存结构」解耦。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredQueueItem {
    pub path: String,
    pub duration: f64,
    /// 状态：`"pending"` / `"done"` / `"failed"` / `"running"`（字符串而非枚举，
    /// 见上：新增状态时老文件仍可解析，未知值一律当 pending）
    pub status: String,
}

/// 磁盘格式的版本号。读取时若 `version` 不匹配，**丢弃并返回空队列**而不是猜测——
/// 猜错的后果是用户看到一堆状态错乱的条目。
pub const QUEUE_FILE_VERSION: u32 = 1;

/// 队列文件名（与 config.toml 同目录）。
pub const QUEUE_FILE_NAME: &str = "batch_queue.json";

/// 磁盘上的顶层结构。`items` 允许缺省（老文件 / 手写的空壳），缺省即空队列。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct QueueFile {
    version: u32,
    #[serde(default)]
    items: Vec<StoredQueueItem>,
}

/// 计算同目录临时文件名：在完整文件名后追加 `.tmp`。
///
/// 刻意不用 `Path::with_extension`：`with_extension("json.tmp")` 对
/// `batch_queue.json` 恰好也得到 `batch_queue.json.tmp`，但对**没有扩展名**的路径
/// （或文件名里带点的怪路径）行为会变，容易把临时文件写到别处、或与真实文件同名。
/// 追加字符串是唯一在所有文件名下都稳定的做法。
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

/// 原子写：先写 `<path>.tmp` 再 rename 覆盖。
///
/// 为什么必须原子：进程被强杀时，直接 `fs::write` 会把原文件截断成 0 字节，
/// 用户上次排好的队列就没了——而「被强杀」正是本功能最需要兜住的场景。
pub fn save_queue(path: &Path, items: &[StoredQueueItem]) -> Result<()> {
    // 目标目录可能还不存在（首次运行、或 config.toml 所在目录被用户清过），
    // 不先建目录的话 `fs::write` 会以「找不到路径」失败，队列就永远存不上。
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建队列目录失败: {:?}", parent))?;
        }
    }

    let file = QueueFile {
        version: QUEUE_FILE_VERSION,
        items: items.to_vec(),
    };
    // 紧凑 JSON（`to_string` 而非 `to_string_pretty`）：队列可能上百条，换行缩进只是
    // 白白撑大文件、拖慢读写，没有人工编辑的诉求。
    let content = serde_json::to_string(&file).with_context(|| "序列化批量队列失败")?;

    let tmp = tmp_path(path);
    fs::write(&tmp, content.as_bytes())
        .with_context(|| format!("写入队列临时文件失败: {:?}", tmp))?;

    // rename 在同一目录内是原子的：要么是旧内容、要么是新内容，绝不会是半截。
    // 失败时主动清掉临时文件，避免在用户目录里留下垃圾（下一次写入还会覆盖它，
    // 但留着容易让人误会文件已经落盘）。
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("覆盖队列文件失败: {:?}", path));
    }
    Ok(())
}

/// 读取队列。
///
/// **任何异常都返回空 `Vec` 而不是 Err**：队列是可选状态，一个损坏的文件绝不能让
/// 程序启动失败。这条要写进文档——调用点在启动路径上。
///
/// 只读**真实路径**，从不读 `<path>.tmp`：上次崩溃可能留下一个只写了一半的临时
/// 文件，把它当成队列采纳，用户看到的就是残缺/错乱的条目。半成品永远不能上位。
pub fn load_queue(path: &Path) -> Vec<StoredQueueItem> {
    // 缺失、无权限、非 UTF-8 字节……全部在 `read_to_string` 处变成 Err，统一落到
    // 空队列。这里刻意不用 `?`：调用点在启动路径上，不允许向上传播。
    let content = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };

    // 空文件、半截 JSON、形状不对的 JSON，都在这里解析失败 -> 空队列。
    let file = match serde_json::from_str::<QueueFile>(&content) {
        Ok(parsed) => parsed,
        Err(_) => return Vec::new(),
    };

    // 版本不匹配宁可丢数据也不猜：猜错会让用户看到状态错乱的条目，比「队列没了」
    // 更难排查，也更容易让人误以为文件被写坏了。
    if file.version != QUEUE_FILE_VERSION {
        return Vec::new();
    }

    file.items
}

/// 状态字符串 ↔ 是否「已完成」的判定（界面用它显示进度）。
///
/// 大小写不敏感且忽略首尾空白：状态是我们自己写的，但用户/外部工具可能手工改过
/// 这个文件，多一个空格不该让一条已完成的任务重新变回待处理。
pub fn is_finished(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("done")
}

/// 给界面的一句话摘要：`"队列 30 项（已完成 12）"` / `"队列为空"`。
pub fn describe(items: &[StoredQueueItem]) -> String {
    if items.is_empty() {
        return "队列为空".to_string();
    }
    let finished = items.iter().filter(|i| is_finished(&i.status)).count();
    format!("队列 {} 项（已完成 {}）", items.len(), finished)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个独占的临时目录。用 `进程号 + tag` 命名，避免并发跑测试时互相踩；
    /// 开头先删一次，兜住上一次测试异常退出留下的残骸。**绝不写进仓库树**。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("v2w_queue_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("创建测试临时目录");
        dir
    }

    /// 造一条测试记录，减少每个用例里的样板。
    fn item(path: &str, duration: f64, status: &str) -> StoredQueueItem {
        StoredQueueItem {
            path: path.to_string(),
            duration,
            status: status.to_string(),
        }
    }

    /// 往返：保存再读回来必须**逐字段一致**，包括中文路径、带空格与反斜杠的 Windows
    /// 路径。任何一处转义/编码出错，用户重启后看到的路径就会指向不存在的文件。
    #[test]
    fn roundtrip_preserves_cjk_and_odd_paths() {
        let dir = temp_dir("roundtrip");
        let file = dir.join(QUEUE_FILE_NAME);
        let items = vec![
            item(r"C:\讲座\第一讲 绪论.mp4", 3612.5, "done"),
            item(r"D:\My Files\a b c\第二讲.mp4", 0.0, "pending"),
            item("相对/路径 带空格.wav", 12.25, "failed"),
        ];

        save_queue(&file, &items).expect("保存队列");
        let loaded = load_queue(&file);
        assert_eq!(loaded, items, "往返后必须逐字段一致");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 文件不存在：这是首次启动的正常情况，必须安静地给出空队列。
    #[test]
    fn missing_file_loads_as_empty() {
        let dir = temp_dir("missing");
        let file = dir.join(QUEUE_FILE_NAME);

        assert!(load_queue(&file).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 0 字节文件：强杀留下的空壳。解析必然失败，但绝不能 panic 或返回 Err。
    #[test]
    fn empty_file_loads_as_empty() {
        let dir = temp_dir("empty");
        let file = dir.join(QUEUE_FILE_NAME);
        fs::write(&file, b"").expect("写入空文件");

        assert!(load_queue(&file).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 非 UTF-8 的垃圾字节：`read_to_string` 会失败，同样要落到空队列。
    /// 顺带覆盖「二进制文件被误命名为 json」这种用户手工搞坏的情况。
    #[test]
    fn garbage_bytes_load_as_empty() {
        let dir = temp_dir("garbage");
        let file = dir.join(QUEUE_FILE_NAME);
        fs::write(&file, [0xFF, 0xFE, 0x00, 0x01, 0x80]).expect("写入垃圾字节");

        assert!(load_queue(&file).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 能解析成 JSON、但形状不对（`items` 不是数组）：也要当空队列，而不是让
    /// 反序列化错误往上冒。
    #[test]
    fn valid_json_with_wrong_shape_loads_as_empty() {
        let dir = temp_dir("shape");
        let file = dir.join(QUEUE_FILE_NAME);
        fs::write(&file, br#"{"version":1,"items":"not-an-array"}"#).expect("写入错误形状");

        assert!(load_queue(&file).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 版本号不匹配：宁可丢弃也不猜测。用比当前版本大的号模拟「未来格式」。
    #[test]
    fn wrong_version_loads_as_empty() {
        let dir = temp_dir("version");
        let file = dir.join(QUEUE_FILE_NAME);
        let raw = r#"{"version":999,"items":[{"path":"a.mp4","duration":1.0,"status":"done"}]}"#;
        fs::write(&file, raw).expect("写入未来版本");

        assert!(load_queue(&file).is_empty(), "版本不匹配必须丢弃");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 未知状态字符串：条目要**保留**，status 原样带出（不翻译、不猜测），并且
    /// 按「未完成」对待。这样将来新增状态时，老程序也不会把条目吃掉。
    #[test]
    fn unknown_status_is_kept_and_not_finished() {
        let dir = temp_dir("unknown_status");
        let file = dir.join(QUEUE_FILE_NAME);
        let items = vec![item("x.mp4", 3.0, "paused_by_user")];

        save_queue(&file, &items).expect("保存队列");
        let loaded = load_queue(&file);

        assert_eq!(loaded, items, "未知状态必须原样保留");
        assert!(!is_finished(&loaded[0].status), "未知状态不算已完成");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 父目录不存在时 `save_queue` 要自己建出来：首次运行时配置目录可能还没生成，
    /// 这里失败就意味着队列永远存不上。
    #[test]
    fn save_creates_missing_parent_dirs() {
        let dir = temp_dir("parents");
        let file = dir.join("nested").join("deeper").join(QUEUE_FILE_NAME);
        assert!(!file.parent().expect("有父目录").exists());

        save_queue(&file, &[item("a.mp4", 1.0, "pending")]).expect("应自动创建父目录");
        assert!(file.exists(), "文件必须真的落盘");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 成功保存后不能留下 `.tmp`：残留的临时文件会让人误以为写盘没完成，
    /// 也让「崩溃遗留」的排查失去信号。
    #[test]
    fn successful_save_leaves_no_tmp_file() {
        let dir = temp_dir("no_tmp");
        let file = dir.join(QUEUE_FILE_NAME);

        save_queue(&file, &[item("a.mp4", 1.0, "pending")]).expect("保存队列");

        assert!(!tmp_path(&file).exists(), "成功写盘后不应残留 .tmp");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 崩溃遗留的 `.tmp` 不能影响读取：真实文件是好的，就必须读到好的那份。
    /// 若实现里把 `.tmp` 也纳入读取候选，这条会失败。
    #[test]
    fn stale_tmp_file_does_not_affect_load() {
        let dir = temp_dir("stale_tmp");
        let file = dir.join(QUEUE_FILE_NAME);
        let items = vec![item("good.mp4", 5.0, "done")];
        save_queue(&file, &items).expect("保存队列");

        // 模拟上次写入写到一半就被强杀：临时文件存在但内容残缺。
        fs::write(tmp_path(&file), b"{\"version\":1,\"items\":[{\"path\"").expect("写入半截 tmp");

        assert_eq!(load_queue(&file), items, "读取必须只看真实文件");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `is_finished` 真值表：`done` 及大小写/空白变体为真，其余（含未知串）为假。
    #[test]
    fn is_finished_truth_table() {
        for yes in ["done", "DONE", "Done", "  done  ", "\tdone\n"] {
            assert!(is_finished(yes), "{yes:?} 应判为已完成");
        }
        for no in [
            "pending",
            "failed",
            "running",
            "dOne?",
            "donex",
            "d",
            "",
            "  ",
            "已完成",
        ] {
            assert!(!is_finished(no), "{no:?} 不应判为已完成");
        }
    }

    /// `describe` 的文案必须精确（界面直接显示），且完成数按 `is_finished` 统计。
    #[test]
    fn describe_reports_exact_strings() {
        assert_eq!(describe(&[]), "队列为空");

        let items = vec![
            item("a.mp4", 1.0, "done"),
            item("b.mp4", 1.0, "DONE"),
            item("c.mp4", 1.0, "pending"),
            item("d.mp4", 1.0, "unknown"),
        ];
        assert_eq!(describe(&items), "队列 4 项（已完成 2）");

        let all_done = vec![item("a.mp4", 1.0, "done"), item("b.mp4", 1.0, "done")];
        assert_eq!(describe(&all_done), "队列 2 项（已完成 2）");
    }

    /// 永不 panic：把一批畸形输入喂给两个入口，只要不炸就算过。
    /// 覆盖目录被当文件读、超长路径、空路径、含控制字符的路径等边界。
    #[test]
    fn never_panics_on_hostile_inputs() {
        let dir = temp_dir("hostile");
        let file = dir.join(QUEUE_FILE_NAME);

        // 把目录本身当队列文件读：read_to_string 失败 -> 空。
        assert!(load_queue(&dir).is_empty());
        // 空路径、不存在的深路径。
        assert!(load_queue(Path::new("")).is_empty());

        // 控制字符 / 换行 / 引号 / 反斜杠结尾的路径，序列化与回读都不应炸。
        let nasty = vec![
            item("line\nbreak\tand\"quote\".mp4", -1.0, "done"),
            item(r"C:\trailing\backslash\", 0.0, "pending"),
            item("", f64::MAX, "failed"),
            item(&"x".repeat(4096), 1.0, "done"),
        ];
        save_queue(&file, &nasty).expect("保存畸形路径");
        assert_eq!(load_queue(&file), nasty, "畸形路径也要能原样往返");

        let _ = fs::remove_dir_all(&dir);
    }
}
