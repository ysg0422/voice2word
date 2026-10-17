//! 项目状态打包 / 还原（纯文件管道，无 UI、无 SQLite API）。
//!
//! # 为什么需要这个模块
//!
//! 本应用把「属于自己的东西」全部放在项目根下：
//!
//! - `voice2word.db`：**全部**任务与字幕库（一个 SQLite 文件）；
//! - `config.toml`：通用配置（术语表 / glossary 也在里面）；
//! - `config.local.toml`：本机专属路径（ffmpeg、模型目录等）。
//!
//! 于是有两类用户会需要「快照」：
//!
//! 1. **换机器**：要把整套状态搬到新机器，只能手工拷文件；
//! 2. **做危险操作之前**（批量重跑、清理数据、换版本）：想留一个可回退的点。
//!
//! 手工拷贝在应用运行时是**不安全**的：SQLite 开了 WAL 后，最近的写入可能还在
//! `voice2word.db-wal` 里没合并进主库，此刻按文件复制只会拿到一个**写到一半的主库**
//! （甚至是不含最近几次编辑的旧主库），拷出来的备份看起来成功、实际是坏的。
//! 更糟的是「读主库、再读 `-wal`、再读 `-shm`」这三次读之间没有任何快照点：应用
//! 正好在这中间 checkpoint 一次，归档里就是「新主库 + 旧 WAL」的撕裂组合，恢复后
//! `quick_check` 不过，库会被隔离成一个空库——用户以为备份救了他，实际什么都没了。
//!
//! 因此数据库**不裸读**，而是让 SQLite 自己产出一份一致快照：只读连接上执行
//! `VACUUM INTO '<临时文件>'`（见 [`snapshot_database`]），把快照文件放进归档。
//! 快照是一个**独立、自洽的单文件库**：既包含 WAL 里尚未合并的写入，也不再需要
//! `-wal` / `-shm` 陪伴，恢复时直接落一个 `.db` 即可。
//!
//! 本模块因此产出**一个带时间戳的归档**，用户可以把它当作一个文件保存 / 搬运 / 回退。
//!
//! # 职责边界
//!
//! 这里只做「取快照 + 文件搬运」：对数据库只调用 `VACUUM INTO`（不改源库一个字节，
//! 连接是只读的），其余是读文件、写 zip、再写回文件。不认识「任务」是什么，不弹界面。
//! 好处是这一层可以被单测覆盖（临时目录里造几个文件 / 一个真库就能验证全部行为），
//! 而界面 / 业务层只需调用 [`create_backup`] 与 [`restore_backup`] 并展示返回结构。
//!
//! # 为什么手写 STORED zip 而不是加 `zip` crate
//!
//! `Cargo.toml` 里没有 `zip` 依赖（离线缓存里也没有），为这一个功能新增依赖需要
//! 网络 / 缓存运气，且会拖慢全仓编译。归档只是「把几个文件原样塞进一个容器」，
//! 不需要 deflate：这些文件体积不大（`voice2word.db` 通常几 MB），
//! **STORED（不压缩）** 的写入 / 读回各约几十行，CRC-32 用标准表驱动实现即可。
//! `flate2` 虽在依赖里，但压缩会让格式校验变复杂（要额外验 CRC 与解压长度），
//! 收益却很小，所以这里明确选择 STORED，并在注释里标注每条格式字段的含义。
//!
//! # 还原策略：**绝不覆盖已有文件**
//!
//! 还原时若目标已存在，**不写入**，只把名字记进
//! [`RestoreOutcome::skipped_existing`] 交给界面提示。理由是覆盖
//! `voice2word.db` 会毁掉用户**当前的全部工程**——这种操作必须由用户显式先移走 /
//! 重命名旧文件，不能由一次误点触发。宁可让用户多走一步，也不让一次误操作
//! 变成不可逆的数据丢失。

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

/// 备份里包含的文件（相对项目根）。顺序即写入归档的顺序。
pub const BACKUP_ENTRIES: [&str; 3] = [DB_ENTRY, "config.toml", "config.local.toml"];

/// 数据库在归档里的条目名（也是项目根下的默认文件名）。
const DB_ENTRY: &str = "voice2word.db";

/// 一次备份的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupOutcome {
    /// 归档文件路径
    pub archive: PathBuf,
    /// 实际写进归档的条目（`BACKUP_ENTRIES` 里不存在的会被跳过）
    pub included: Vec<String>,
    /// 归档字节数
    pub bytes: u64,
}

/// 还原结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// 实际写回的文件名
    pub restored: Vec<String>,
    /// 因为目标已存在而被跳过的文件名
    pub skipped_existing: Vec<String>,
}

/// 把数据目录里的关键文件打包成一个 zip 归档。
///
/// - `root`：项目根（配置文件从 `root` 下取；数据库默认是 `root/voice2word.db`）。
/// - `dest`：归档落点（例如 `root/backups/voice2word-backup-<时间戳>.zip`）；
///   父目录不存在会自动创建。**同秒重名不覆盖**：若 `dest` 已存在（连点两次
///   备份、或两个窗口同时备份），自动改用 `-1` / `-2`… 后缀的派生文件名，
///   并把它作为 [`BackupOutcome::archive`] 返回（调用方据此展示真实落点）。
/// - 数据库先经 [`snapshot_database`] 取**一致快照**，再把快照文件放进归档；
///   **不再**打包 `-wal` / `-shm`——快照本身已包含 WAL 里未合并的写入，
///   再带上伴随文件反而会把两份快照混在一起。
/// - 不存在的条目**跳过并记日志**，不算失败（瘦包里可能还没有 `config.local.toml`）；
///   但**一个都没有**时返回 `Err`（空归档没有任何意义，静默产出一个 0 条目的 zip
///   会让用户以为备份成功了）。
/// - 快照失败（库被锁 / 损坏 / 打不开）是**硬错误**，绝不退回裸读文件的老路：
///   一份可能撕裂的备份比「明确告诉你备份失败」危险得多。
///
/// 旧签名保持不变，等价于
/// `create_backup_with_db(root, &root.join("voice2word.db"), dest)`。
///
/// 以排他方式在 `dest`（或其派生别名）上创建一个新归档文件。
///
/// 返回实际落盘路径与已打开的句柄。`dest` 已被占用时依次尝试
/// `stem-1.ext`、`stem-2.ext`…（最多 1000 个），保证同一秒内的第二次备份
/// 不会覆盖第一份。用 `create_new` 而不是「先 `exists` 再 `write`」：存在性
/// 检查与创建必须是**一个原子操作**，否则两个进程同时备份仍会互相覆盖。
fn create_unique_archive_file(dest: &Path) -> Result<(PathBuf, std::fs::File)> {
    for attempt in 0..=1000u32 {
        let candidate = if attempt == 0 {
            dest.to_path_buf()
        } else {
            let stem = dest
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("voice2word-backup");
            let name = match dest.extension().and_then(|ext| ext.to_str()) {
                Some(ext) => format!("{stem}-{attempt}.{ext}"),
                None => format!("{stem}-{attempt}"),
            };
            dest.with_file_name(name)
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("创建归档文件失败: {}", candidate.display()));
            }
        }
    }
    bail!(
        "备份归档重名过多，写入失败: {}（同秒内的同名文件超过 1000 个）",
        dest.display()
    );
}

pub fn create_backup(root: &Path, dest: &Path) -> Result<BackupOutcome> {
    // 数据库默认就在项目根：界面层旧调用点只需传 root，行为与改前一致。
    create_backup_with_db(root, &root.join(DB_ENTRY), dest)
}

/// 同 [`create_backup`]，但数据库位置由调用方**显式**给出。
///
/// 为什么需要它：库路径由 `AppConfig::resolve_path("voice2word.db")` 解析（项目根
/// 可能来自 `VOICE2WORD_HOME` 等覆盖），而归档落点 `root` 与库所在目录未必相同。
/// 把路径作为参数传进来，比在备份模块里再实现一遍根解析更可靠——根解析只应有
/// 一处实现，且备份模块不该读环境变量。
pub fn create_backup_with_db(root: &Path, db_path: &Path, dest: &Path) -> Result<BackupOutcome> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建备份目录失败: {}", parent.display()))?;
        }
    }

    // 先收集 (归档内名字, 文件字节)。配置文件按 BACKUP_ENTRIES 顺序原样读；
    // 数据库走「先快照、再读快照字节」，绝不直接读活着的 `.db` / `-wal` / `-shm`。
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut included: Vec<String> = Vec::new();

    // 快照临时目录按需创建（库不存在时不必碰 %TEMP%），由 RAII 负责清理。
    let mut snapshot_dir: Option<SnapshotDir> = None;

    for name in BACKUP_ENTRIES {
        if name == DB_ENTRY {
            if !db_path.is_file() {
                // 与旧行为一致：瘦包 / 首次运行的目录里可能还没有库，跳过而非失败。
                warn!(entry = name, db = %db_path.display(), "备份数据库不存在，已跳过");
                continue;
            }
            if snapshot_dir.is_none() {
                snapshot_dir = Some(SnapshotDir::new()?);
            }
            let dir = snapshot_dir.as_ref().expect("刚创建");
            let snapshot = dir.path.join("snapshot.db");
            snapshot_database(db_path, &snapshot)?;
            let bytes = std::fs::read(&snapshot)
                .with_context(|| format!("读取数据库快照失败: {}", snapshot.display()))?;
            entries.push((name.to_string(), bytes));
            included.push(name.to_string());
            continue;
        }

        // `config.toml` / `config.local.toml` 不是数据库，保持原来的原样拷贝。
        let path = root.join(name);
        if path.is_file() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("读取备份源文件失败: {}", path.display()))?;
            entries.push((name.to_string(), bytes));
            included.push(name.to_string());
        } else {
            // 跳过而不是失败：瘦包 / 新装环境本就没有 config.local.toml。
            // 但一定要打日志，避免「备份成功但少文件」变成无声事实。
            warn!(entry = name, root = %root.display(), "备份条目不存在，已跳过");
        }
    }

    if included.is_empty() {
        // 静默产出一个 0 条目的 zip 会让用户以为备份成功了，这是最坏的失败模式：
        // 等到需要回退时才发现备份是空的。因此这里必须是硬错误。
        bail!(
            "备份失败：{} 下没有任何可备份的文件（{}）",
            root.display(),
            BACKUP_ENTRIES.join("、")
        );
    }

    let bytes = build_stored_zip(&entries)?;
    // 同名归档可能已经在同秒里落盘：覆盖会静默毁掉前一份，改走派生文件名。
    let (dest, mut file) = create_unique_archive_file(dest)?;
    if let Err(e) = file.write_all(&bytes) {
        // 写失败（磁盘满 / 被中断）时删掉刚独占创建的半成品：留在 `backups/` 里的
        // 0 字节或截断 `.zip` 会被用户当成一份有效备份，日后挑它恢复必然失败，
        // 比「这次备份失败了」难排查得多。
        drop(file);
        let _ = std::fs::remove_file(&dest);
        return Err(e).with_context(|| format!("写入归档失败: {}", dest.display()));
    }

    info!(
        archive = %dest.display(),
        included = included.len(),
        bytes = bytes.len(),
        "备份归档已生成"
    );

    Ok(BackupOutcome {
        archive: dest.to_path_buf(),
        included,
        bytes: bytes.len() as u64,
    })
}

/// 快照临时目录的 RAII 守卫：离开作用域即整目录删除。
///
/// 为什么不用「用完手动删」：快照之后还有读字节、建 zip、写归档几步，任何一步
/// `?` 提前返回都会跳过手写清理，在 `%TEMP%` 里留下一个与库等大的 `.db`。
struct SnapshotDir {
    path: PathBuf,
}

impl SnapshotDir {
    fn new() -> Result<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "voice2word-backup-snapshot-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("创建快照临时目录失败: {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for SnapshotDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 用 `VACUUM INTO` 生成数据库的**一致快照**（写入 `out`；该文件必须不存在）。
///
/// # 为什么是 `VACUUM INTO` 而不是拷文件
///
/// - 它在源库上开一个**读事务**：快照对应「开始读的那一刻」的完整状态，运行中的
///   写入不会把主库 / `-wal` / `-shm` 撕成互不一致的三份（这正是旧实现
///   「三次 `fs::read` 之间没有快照点」会产出的撕裂备份）；
/// - WAL 里尚未合并的写入会被包含进去，所以恢复后不丢最后几次编辑；
/// - 产物是**自洽单文件**，不需要 `-wal` / `-shm` 陪伴，也就不会出现「旧快照的 WAL
///   落到新库旁边」的混搭。
///
/// 连接**只读**打开：备份这条路不该有任何机会修改用户正在用的库。
/// 失败（库被锁、损坏、根本不是 SQLite 文件）一律向上返回明确错误，**绝不**退回
/// 裸读——那正是本模块要消灭的撕裂备份来源。
fn snapshot_database(db_path: &Path, out: &Path) -> Result<()> {
    use rusqlite::OpenFlags;

    // `to_string_lossy` 是这里唯一可行的传参方式（SQLite 的 VACUUM INTO 只收
    // UTF-8 路径）。若路径含无法编码的字节，SQLite 会以「无法创建文件」报错，
    // 仍然落在下面这条明确的错误信息里，而不是静默产出坏备份。
    let out_str = out.to_string_lossy().to_string();
    let conn = rusqlite::Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("以只读方式打开数据库失败: {}", db_path.display()))?;
    // 与 `storage::db` 的 5 s 窗口保持一致：用户很可能在转写进行中点备份，
    // 短暂等锁比直接判失败更贴合真实场景；超时则返回明确错误。
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    conn.execute("VACUUM INTO ?1", rusqlite::params![out_str])
        .with_context(|| {
            format!(
                "生成数据库一致快照失败（库可能被其他进程锁定，或文件已损坏）。\
                 为避免产出撕裂的备份，本次备份已中止: {}",
                db_path.display()
            )
        })?;
    if !out.is_file() {
        bail!(
            "生成数据库快照失败：SQLite 未产出快照文件（{}）",
            out.display()
        );
    }
    Ok(())
}

/// 从归档恢复：把归档里的条目写回 `root`。
///
/// - **不覆盖已有文件**：目标已存在时**不**写入，并把它记进
///   [`RestoreOutcome::skipped_existing`] 返回给调用方（界面据此提示
///   「这些文件已存在，已保留现有版本」）。理由：覆盖 `voice2word.db` 会毁掉
///   用户当前的全部工程，这种操作必须由用户显式先移走/重命名旧文件，
///   不能由一次误点触发。
/// - **按组判断**：主库与它的 `-wal` / `-shm` 属于同一份快照，组内**任一**名字
///   已存在就整组跳过。否则「主库已存在（跳过）+ 伴随文件不存在（写入）」会把旧
///   快照的 WAL 留在用户当前的库旁边，下次启动由 SQLite 回放进当前主库。
///   新归档里只有一份自洽的 `.db` 快照（不带伴随文件），但**旧版本产出的归档**里
///   可能还有 `-wal` / `-shm`，这条组保护必须继续生效。
/// - 归档里不认识的条目忽略（不写入、不报错）。
/// - 归档损坏 / 不是 zip → `Err`。
pub fn restore_backup(archive: &Path, root: &Path) -> Result<RestoreOutcome> {
    let bytes =
        std::fs::read(archive).with_context(|| format!("读取归档失败: {}", archive.display()))?;
    let files = read_stored_zip(&bytes)?;

    std::fs::create_dir_all(root)
        .with_context(|| format!("创建还原目录失败: {}", root.display()))?;

    let mut restored = Vec::new();
    let mut skipped_existing = Vec::new();

    // 「还原**开始前**目标目录里已存在哪些组」的快照。
    //
    // 必须在写任何文件之前算好：下面每写一个成员，组内其它成员就会看到它进而
    // 误判成「该组已存在」，结果是同一组只还原了一半（先写的进、后写的被跳），
    // 恰好制造出本函数要防的那种「混着两份快照」的局面。
    let blocked_groups: Vec<&'static str> = BACKUP_ENTRIES
        .iter()
        .copied()
        .filter(|group| group_has_existing_member(root, group))
        .collect();

    for (name, data) in &files {
        // 只认自己认识的条目：归档里多出来的东西（未来版本 / 手工塞的）一律忽略，
        // 既避免往项目根里写陌生文件，也避免旧版本读到新格式时误解。
        let Some(group) = entry_group(name) else {
            continue;
        };
        // 关键安全阀（按**组**判断）：只要目标目录里已经存在组内任何一个文件名，
        // 就整组放弃还原。
        //
        // 为什么不能逐个条目判断：`voice2word.db` 存在（跳过）而 `voice2word.db-wal`
        // 不存在（当前库已 checkpoint，伴随文件被合并走了）时，逐条判断会把
        // **旧快照的 WAL** 写进当前目录；下次启动 SQLite 会拿它回放当前主库，
        // 轻则判损坏、重则把旧数据混进用户当前的库里。而界面此刻报的是
        // 「已恢复 N 个文件」——一次典型的假成功。
        if blocked_groups.contains(&group) {
            warn!(target = %root.join(name).display(), "还原目标（或同组文件）已存在，保留现有版本");
            skipped_existing.push(name.clone());
            continue;
        }
        // `create_new` 把「检查存在 + 创建」做成一次原子操作：`fs::write` 会**先
        // 截断**目标，一旦与上面的检查之间存在窗口，用户的实时库就被清空了。
        if write_new_file(&root.join(name), data)? {
            restored.push(name.clone());
        } else {
            warn!(target = %root.join(name).display(), "还原目标已存在，保留现有版本");
            skipped_existing.push(name.clone());
        }
    }

    Ok(RestoreOutcome {
        restored,
        skipped_existing,
    })
}

/// 该组的**任一**成员（本体 / `-wal` / `-shm`）是否已存在于 `root` 下。
///
/// 刻意检查全部三个可能的名字，而不是「归档里出现过的那些」：归档只带了 `-wal`
/// 而目标目录里已有主库时，同样属于「绝不能把两份快照混在一起」。
fn group_has_existing_member(root: &Path, group: &str) -> bool {
    [
        group.to_string(),
        format!("{group}-wal"),
        format!("{group}-shm"),
    ]
    .iter()
    .any(|member| root.join(member).exists())
}

/// 以 `create_new` 落地一个还原文件；目标已存在时返回 `Ok(false)`（绝不覆盖）。
///
/// 为什么不用「先 `exists` 再 `fs::write`」：两者之间隔着一个系统调用窗口，而
/// `fs::write` 会**先截断**目标文件——正好破坏「绝不覆盖用户当前数据」这条最关键
/// 的承诺（主库被截断即等于用户全部工程消失）。`create_new` 让「检查 + 创建」原子
/// 完成，把这种破坏从「取决于时序」降为「不可能」。
///
/// 写入中途失败时不留下半截文件：目标是我们刚创建的，直接删掉远好过留一个
/// 「看起来已还原、实际残缺」的库。
fn write_new_file(target: &Path, data: &[u8]) -> Result<bool> {
    use std::io::Write;

    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => {
            return Err(e).with_context(|| format!("创建还原文件失败: {}", target.display()));
        }
    };
    if let Err(e) = file.write_all(data) {
        drop(file);
        let _ = std::fs::remove_file(target);
        return Err(e).with_context(|| format!("写入还原文件失败: {}", target.display()));
    }
    Ok(true)
}

/// 备份文件名：`voice2word-backup-YYYYMMDD-HHMMSS.zip`（本地时间）。
///
/// 抽成纯函数便于单测（不依赖时钟）。同秒第二次备份的重名由
/// [`create_backup`] 自动加 `-1`/`-2`… 后缀消解——重名探测涉及文件系统，
/// 刻意不放进这个纯函数，保证它永远只由时间决定。
pub fn backup_file_name(now: chrono::DateTime<chrono::Local>) -> String {
    now.format("voice2word-backup-%Y%m%d-%H%M%S.zip")
        .to_string()
}

/// 条目名属于哪一组（返回组的主文件名，例如 `voice2word.db`）；不认识的条目返回
/// `None`。主库与它的 `-wal` / `-shm` 是**同一份快照的三个部分**，还原时按组处理，
/// 理由见 [`restore_backup`]。调用方用 `None` 表示「归档里多出来的东西」，一律忽略：
/// 既避免往项目根里写陌生文件，也避免旧版本读到新格式时误解。
fn entry_group(name: &str) -> Option<&'static str> {
    BACKUP_ENTRIES.iter().copied().find(|base| {
        name == *base || name == format!("{base}-wal") || name == format!("{base}-shm")
    })
}

// ---------------------------------------------------------------------------
// 手写 STORED zip（无压缩）——见模块文档里的选型说明
// ---------------------------------------------------------------------------

/// 本地文件头签名 `PK\x03\x04`
const SIG_LOCAL: u32 = 0x0403_4b50;
/// 中央目录文件头签名 `PK\x01\x02`
const SIG_CENTRAL: u32 = 0x0201_4b50;
/// 中央目录结束记录签名 `PK\x05\x06`
const SIG_EOCD: u32 = 0x0605_4b50;
/// 压缩方法 0 = 存储（不压缩）
const METHOD_STORED: u16 = 0;

/// 标准表驱动 CRC-32（反射多项式 0xEDB88320，即 zip / PNG / gzip 用的那种）。
///
/// zip 的每个条目头都必须带内容的 CRC-32，解压器据此判断数据有没有坏。
/// 自己实现而不是引依赖，理由见模块文档。
fn crc32(data: &[u8]) -> u32 {
    // 惰性构造查表（首次使用时算一次，常驻静态内存）。
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });

    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// 把 `(归档内名字, 内容)` 序列化成 STORED zip 字节。
fn build_stored_zip(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    // 条目数（u16）与总长度（u32 偏移）都是格式硬上限，且必须在写头**之前**校验，
    // 理由见 [`check_zip_limits`]。
    check_zip_limits(0, files.len())?;

    let mut out: Vec<u8> = Vec::new();
    // 每个条目的 (名字, crc, 大小, 本地头偏移)，供中央目录复用。
    let mut central_meta: Vec<(String, u32, u32, u32)> = Vec::new();

    for (name, data) in files {
        // 4GB 是 STORED zip（非 Zip64）的硬上限，写进去会静默截断，必须拦住。
        if data.len() as u64 > u32::MAX as u64 {
            bail!("条目过大，超出 zip 格式上限: {name}");
        }
        if name.len() > u16::MAX as usize {
            bail!("条目名过长: {name}");
        }
        check_zip_limits(out.len(), files.len())?;
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;
        let name_bytes = name.as_bytes();

        // ---- 本地文件头 ----
        out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // 解压所需版本
        out.extend_from_slice(&0u16.to_le_bytes()); // 通用标志位（无加密 / 无 data descriptor）
        out.extend_from_slice(&METHOD_STORED.to_le_bytes()); // 压缩方法：存储
        out.extend_from_slice(&0u16.to_le_bytes()); // 最后修改时间（备份内不承载时间语义）
        out.extend_from_slice(&0u16.to_le_bytes()); // 最后修改日期
        out.extend_from_slice(&crc.to_le_bytes()); // 内容 CRC-32
        out.extend_from_slice(&size.to_le_bytes()); // 压缩后大小 == 原始大小（STORED）
        out.extend_from_slice(&size.to_le_bytes()); // 原始大小
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes()); // 名字长度
        out.extend_from_slice(&0u16.to_le_bytes()); // 扩展字段长度
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data); // STORED：内容原样跟在头后面

        central_meta.push((name.clone(), crc, size, offset));
    }

    // 中央目录自己的偏移也是 u32：超过 4 GiB 时 `as u32` 会回绕，产出一个能写盘、
    // 但中央目录指向错误位置的**损坏归档**，而调用方拿到的是 `Ok`（假成功）。
    check_zip_limits(out.len(), files.len())?;
    let central_offset = out.len() as u32;

    // ---- 中央目录 ----
    for (name, crc, size, offset) in &central_meta {
        let name_bytes = name.as_bytes();
        out.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // 创建版本
        out.extend_from_slice(&20u16.to_le_bytes()); // 解压所需版本
        out.extend_from_slice(&0u16.to_le_bytes()); // 通用标志位
        out.extend_from_slice(&METHOD_STORED.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // 时间
        out.extend_from_slice(&0u16.to_le_bytes()); // 日期
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes()); // 压缩后大小
        out.extend_from_slice(&size.to_le_bytes()); // 原始大小
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // 扩展字段长度
        out.extend_from_slice(&0u16.to_le_bytes()); // 注释长度
        out.extend_from_slice(&0u16.to_le_bytes()); // 起始磁盘号（单卷固定 0）
        out.extend_from_slice(&0u16.to_le_bytes()); // 内部属性
        out.extend_from_slice(&0u32.to_le_bytes()); // 外部属性
        out.extend_from_slice(&offset.to_le_bytes()); // 本地头偏移
        out.extend_from_slice(name_bytes);
    }

    let central_size = out.len() as u32 - central_offset;
    let count = central_meta.len() as u16;

    // ---- 中央目录结束记录（EOCD）----
    out.extend_from_slice(&SIG_EOCD.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // 本磁盘号
    out.extend_from_slice(&0u16.to_le_bytes()); // 中央目录起始磁盘号
    out.extend_from_slice(&count.to_le_bytes()); // 本磁盘条目数
    out.extend_from_slice(&count.to_le_bytes()); // 总条目数
    out.extend_from_slice(&central_size.to_le_bytes()); // 中央目录字节数
    out.extend_from_slice(&central_offset.to_le_bytes()); // 中央目录偏移
    out.extend_from_slice(&0u16.to_le_bytes()); // 注释长度

    Ok(out)
}

/// 归档尺寸与条目数的格式上限校验（STORED、非 Zip64：字段只有 u32 / u16）。
///
/// 必须在写出任何一个头部**之前**调用：`as u32` 对超过 4 GiB 的偏移会静默回绕，
/// 结果是「写出了一个坏 zip，却返回成功」——用户以为备份好了，等要回退时才发现
/// 归档指向乱处。抽成纯函数是为了能单测：真造一份 4 GiB 的归档来触发它不现实。
fn check_zip_limits(total_bytes: usize, entry_count: usize) -> Result<()> {
    if total_bytes as u64 > u32::MAX as u64 {
        bail!("归档总长度超过 zip 格式上限（4 GiB），无法在不启用 Zip64 的情况下写出");
    }
    if entry_count > u16::MAX as usize {
        bail!("归档条目数超过 zip 格式上限（65535）");
    }
    Ok(())
}

/// 从 STORED zip 字节里读回 `(名字, 内容)` 列表。
///
/// 只支持存储(0)：本模块自己写的归档只用这一种。遇到 deflate 等其它方法
/// 明确报错而不是猜（宁可让用户看到「归档格式不支持」，也不要解出错的数据）。
fn read_stored_zip(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let eocd = find_eocd(bytes)?;
    let total = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]) as usize;
    let central_offset = u32::from_le_bytes([
        bytes[eocd + 16],
        bytes[eocd + 17],
        bytes[eocd + 18],
        bytes[eocd + 19],
    ]) as usize;

    let mut pos = central_offset;
    let mut out = Vec::with_capacity(total);
    for _ in 0..total {
        let sig = read_u32(bytes, pos)?;
        if sig != SIG_CENTRAL {
            bail!("zip 中央目录损坏（签名不符 @{pos}）");
        }
        let method = read_u16(bytes, pos + 10)?;
        if method != METHOD_STORED {
            bail!("归档条目使用了不支持的压缩方法 {method}（仅支持存储）");
        }
        let crc = read_u32(bytes, pos + 16)?;
        let size = read_u32(bytes, pos + 20)?;
        let name_len = read_u16(bytes, pos + 28)? as usize;
        let extra_len = read_u16(bytes, pos + 30)? as usize;
        let comment_len = read_u16(bytes, pos + 32)? as usize;
        let local_offset = read_u32(bytes, pos + 42)? as usize;

        let name_start = pos + 46;
        let name_bytes = slice(bytes, name_start, name_len)?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| anyhow::anyhow!("归档条目名不是合法 UTF-8"))?
            .to_string();

        // 用中央目录里记的本地头偏移去取内容，同时复核 CRC：
        // 数据损坏时这里会先炸，而不是把坏字节写回用户目录。
        let data = read_local_entry(bytes, local_offset, size as usize)?;
        if crc32(data) != crc {
            bail!("归档条目 CRC 校验失败: {name}（文件可能已损坏）");
        }
        out.push((name, data.to_vec()));

        pos = name_start + name_len + extra_len + comment_len;
    }

    if out.is_empty() {
        bail!("归档里没有任何条目");
    }
    Ok(out)
}

/// 读一个本地文件头并切出它的内容字节（STORED：内容紧跟头部）。
fn read_local_entry(bytes: &[u8], offset: usize, size: usize) -> Result<&[u8]> {
    let sig = read_u32(bytes, offset)?;
    if sig != SIG_LOCAL {
        bail!("zip 本地文件头损坏（签名不符 @{offset}）");
    }
    let method = read_u16(bytes, offset + 8)?;
    if method != METHOD_STORED {
        bail!("归档条目使用了不支持的压缩方法 {method}（仅支持存储）");
    }
    let name_len = read_u16(bytes, offset + 26)? as usize;
    let extra_len = read_u16(bytes, offset + 28)? as usize;
    let start = offset + 30 + name_len + extra_len;
    slice(bytes, start, size)
}

/// 从尾部向前找 EOCD（注释最长 65535，所以只需回看这么多字节）。
fn find_eocd(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < 22 {
        bail!("不是合法的 zip（长度不足 EOCD）");
    }
    let min = bytes.len().saturating_sub(22 + 0xffff);
    let mut i = bytes.len() - 22;
    loop {
        if read_u32(bytes, i).map(|v| v == SIG_EOCD).unwrap_or(false) {
            return Ok(i);
        }
        if i == min {
            break;
        }
        i -= 1;
    }
    bail!("未找到 zip 中央目录结束记录（文件可能损坏或截断）")
}

fn slice(b: &[u8], start: usize, len: usize) -> Result<&[u8]> {
    b.get(start..start + len)
        .ok_or_else(|| anyhow::anyhow!("zip 数据越界（@{start} 长度 {len}）"))
}

fn read_u16(b: &[u8], off: usize) -> Result<u16> {
    let s = slice(b, off, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32(b: &[u8], off: usize) -> Result<u32> {
    let s = slice(b, off, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 每个测试用独立临时目录，避免并行跑测试时互相踩。
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("v2w_backup_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 造一个**真实**的 SQLite 库。
    ///
    /// 数据库现在走 `VACUUM INTO` 快照，假字节会被 SQLite 判为
    /// `file is not a database`——旧测试用 `b"db"` 当库内容，改完后必须换成真库，
    /// 否则测的只是「快照失败路径」。
    fn make_db(path: &Path, notes: &[&str]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建库所在目录");
        }
        let conn = rusqlite::Connection::open(path).expect("打开库");
        // 与生产一致用 WAL：快照必须能把 -wal 里尚未合并的写入一起带走。
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (id INTEGER PRIMARY KEY, note TEXT NOT NULL);",
        )
        .expect("建表");
        for note in notes {
            conn.execute(
                "INSERT INTO tasks (note) VALUES (?1)",
                rusqlite::params![note],
            )
            .expect("插行");
        }
    }

    /// 读库里的全部 note。用「数据」而不是「文件字节」验证快照内容：
    /// `VACUUM INTO` 会重排页面，字节不可能与源文件相同。
    fn db_notes(path: &Path) -> Vec<String> {
        let conn = rusqlite::Connection::open(path).expect("打开库");
        let mut stmt = conn
            .prepare("SELECT note FROM tasks ORDER BY id")
            .expect("准备查询");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("查询")
            .map(|r| r.expect("取行"))
            .collect();
        rows
    }

    /// 从归档里取出某个条目（不存在即测试失败）。
    fn archive_entry(archive: &Path, name: &str) -> Vec<u8> {
        read_stored_zip(&std::fs::read(archive).expect("读归档"))
            .expect("解析归档")
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, data)| data)
            .unwrap_or_else(|| panic!("归档里应有 {name}"))
    }

    /// 文件名格式必须是 `voice2word-backup-YYYYMMDD-HHMMSS.zip`，界面直接展示它。
    #[test]
    fn backup_file_name_is_stable() {
        let fixed = chrono::Local
            .with_ymd_and_hms(2026, 10, 9, 8, 7, 6)
            .unwrap();
        assert_eq!(
            backup_file_name(fixed),
            "voice2word-backup-20261009-080706.zip"
        );
    }

    /// 往返：备份 → 清空 root → 还原。数据库内容与配置字节都不能有任何偏差。
    /// 这是本模块存在的意义：换机器 / 回退后数据必须完整可用。
    #[test]
    fn round_trip_restores_database_and_config() {
        let dir = tmp_dir("roundtrip");
        let root = dir.join("root");
        make_db(&root.join("voice2word.db"), &["第一句", "第二句"]);
        std::fs::write(root.join("config.toml"), b"language = \"zh\"\n").unwrap();

        let archive = dir.join("backups").join("snap.zip");
        let outcome = create_backup(&root, &archive).unwrap();
        assert_eq!(outcome.archive, archive);
        assert_eq!(outcome.included, vec!["voice2word.db", "config.toml"]);
        assert!(outcome.bytes > 0);

        // 归档里的库必须是**独立快照**：能单独打开、数据完整，且不带 -wal/-shm。
        let db_bytes = archive_entry(&archive, "voice2word.db");
        let snapshot = dir.join("inspect.db");
        std::fs::write(&snapshot, &db_bytes).unwrap();
        assert_eq!(db_notes(&snapshot), vec!["第一句", "第二句"]);
        assert!(
            read_stored_zip(&std::fs::read(&archive).unwrap())
                .unwrap()
                .iter()
                .all(|(name, _)| !name.ends_with("-wal") && !name.ends_with("-shm")),
            "快照已是自洽单文件，归档里不应再有 -wal/-shm"
        );

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir_all(&root).unwrap();

        let restored = restore_backup(&archive, &root).unwrap();
        assert_eq!(restored.restored, vec!["voice2word.db", "config.toml"]);
        assert!(restored.skipped_existing.is_empty());
        assert_eq!(
            db_notes(&root.join("voice2word.db")),
            vec!["第一句", "第二句"]
        );
        assert_eq!(
            std::fs::read(root.join("config.toml")).unwrap(),
            b"language = \"zh\"\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 瘦包场景：没有 config.local.toml 时应跳过并记日志，不列进 included，但整体仍成功。
    #[test]
    fn missing_entries_are_skipped() {
        let dir = tmp_dir("missing");
        let root = dir.join("root");
        make_db(&root.join("voice2word.db"), &["only"]);

        let archive = dir.join("snap.zip");
        let outcome = create_backup(&root, &archive).unwrap();
        assert!(outcome.included.contains(&"voice2word.db".to_string()));
        assert!(!outcome.included.contains(&"config.local.toml".to_string()));
        assert!(archive.is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **F4 核心回归**：源库正被另一条连接写入（最新数据只在 `-wal` 里）时，
    /// 快照必须包含那次写入，且归档里**不能**再出现 `-wal` / `-shm`。
    ///
    /// 旧实现直接 `fs::read` 三次（主库 / wal / shm），中间没有快照点；一旦应用在
    /// 这期间 checkpoint，归档里就是「新主库 + 旧 WAL」的撕裂组合。
    #[test]
    fn snapshot_includes_live_wal_writes_and_drops_companions() {
        let dir = tmp_dir("wal_snapshot");
        let root = dir.join("root");
        make_db(&root.join("voice2word.db"), &["committed"]);

        // 保持连接存活：这次写入只会落在 -wal 里（SQLite 只在最后一个连接关闭时
        // checkpoint 并删除 -wal），正好模拟「应用正在运行、用户点备份」。
        let live = rusqlite::Connection::open(root.join("voice2word.db")).unwrap();
        let _ = live.pragma_update(None, "journal_mode", "WAL");
        live.execute("INSERT INTO tasks (note) VALUES ('wal-only')", [])
            .expect("写入 WAL");
        assert!(
            root.join("voice2word.db-wal").is_file(),
            "前置条件：最新写入应在 -wal 里"
        );

        let archive = dir.join("snap.zip");
        let outcome = create_backup(&root, &archive).unwrap();
        assert_eq!(
            outcome.included,
            vec!["voice2word.db"],
            "不得再把 -wal/-shm 打进归档"
        );

        let snapshot = dir.join("snapshot.db");
        std::fs::write(&snapshot, archive_entry(&archive, "voice2word.db")).unwrap();
        assert_eq!(
            db_notes(&snapshot),
            vec!["committed", "wal-only"],
            "快照必须包含只存在于 -wal 里的写入"
        );

        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 快照失败（库不是 SQLite / 已损坏）时必须是**明确错误**，不能退回裸读，
    /// 也不能留下一个「看起来成功」的归档。
    #[test]
    fn corrupt_database_is_an_error_not_a_raw_copy() {
        let dir = tmp_dir("corrupt");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("voice2word.db"),
            b"SQLite format 3\0not really a database",
        )
        .unwrap();
        std::fs::write(root.join("config.toml"), b"language = \"zh\"\n").unwrap();

        let archive = dir.join("snap.zip");
        let err = create_backup(&root, &archive).expect_err("坏库必须让备份失败");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("一致快照"),
            "错误信息应说清是快照失败而不是普通 IO: {msg}"
        );
        assert!(!archive.exists(), "失败时不能留下归档（哪怕只含配置文件）");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归档里只有配置、没有库时仍可备份（瘦包首次运行）——库缺失是「跳过」而不是失败。
    #[test]
    fn config_only_backup_is_allowed() {
        let dir = tmp_dir("config_only");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("config.toml"), b"language = \"zh\"\n").unwrap();

        let archive = dir.join("snap.zip");
        let outcome = create_backup(&root, &archive).unwrap();
        assert_eq!(outcome.included, vec!["config.toml"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新签名允许数据库不在 `root` 下：调用方把 `AppConfig::resolve_path` 的结果传进来，
    /// 归档里的条目名仍是 `voice2word.db`（恢复时按这个名字落盘）。
    #[test]
    fn create_backup_with_db_takes_explicit_database_path() {
        let dir = tmp_dir("explicit_db");
        let root = dir.join("data_dir");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = dir.join("elsewhere").join("custom.db");
        make_db(&db_path, &["external"]);

        let archive = dir.join("snap.zip");
        let outcome = create_backup_with_db(&root, &db_path, &archive).unwrap();
        assert_eq!(outcome.included, vec!["voice2word.db"]);

        let snapshot = dir.join("snapshot.db");
        std::fs::write(&snapshot, archive_entry(&archive, "voice2word.db")).unwrap();
        assert_eq!(db_notes(&snapshot), vec!["external"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个源文件都没有时必须报错：静默产出空归档会让用户误以为备份成功。
    #[test]
    fn all_entries_missing_is_error() {
        let dir = tmp_dir("empty");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();

        let archive = dir.join("snap.zip");
        assert!(create_backup(&root, &archive).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 还原绝不覆盖已有文件：目标内容保持原样，名字进 skipped_existing，
    /// 让界面提示「已保留现有版本」。防的是误点毁掉用户当前的全部工程。
    #[test]
    fn restore_does_not_overwrite_existing() {
        let dir = tmp_dir("nooverwrite");
        let root = dir.join("root");
        make_db(&root.join("voice2word.db"), &["OLD-BACKUP-CONTENT"]);

        let archive = dir.join("snap.zip");
        create_backup(&root, &archive).unwrap();

        // 用户在当前工程里继续干活，主库已经变了。
        let live = rusqlite::Connection::open(root.join("voice2word.db")).unwrap();
        live.execute("UPDATE tasks SET note = 'LIVE-CURRENT-DATA'", [])
            .unwrap();
        drop(live);

        let outcome = restore_backup(&archive, &root).unwrap();
        assert!(outcome.restored.is_empty());
        assert_eq!(outcome.skipped_existing, vec!["voice2word.db"]);
        assert_eq!(
            db_notes(&root.join("voice2word.db")),
            vec!["LIVE-CURRENT-DATA"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **旧版本归档**（带 `-wal`/`-shm`）仍必须能整组还原到一个全新目录：
    /// 兼容性不能因为新归档改成单文件快照就丢掉。
    #[test]
    fn legacy_archive_with_companions_restores_whole_group() {
        let dir = tmp_dir("legacy_group");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let bytes = build_stored_zip(&[
            ("voice2word.db".to_string(), b"db".to_vec()),
            ("voice2word.db-wal".to_string(), b"wal-bytes".to_vec()),
            ("voice2word.db-shm".to_string(), b"shm".to_vec()),
        ])
        .unwrap();
        let archive = dir.join("snap.zip");
        std::fs::write(&archive, bytes).unwrap();

        let restored = restore_backup(&archive, &root).unwrap();
        assert_eq!(
            restored.restored,
            vec!["voice2word.db", "voice2word.db-wal", "voice2word.db-shm"]
        );
        assert!(restored.skipped_existing.is_empty());
        assert_eq!(
            std::fs::read(root.join("voice2word.db-wal")).unwrap(),
            b"wal-bytes"
        );
        assert_eq!(
            std::fs::read(root.join("voice2word.db-shm")).unwrap(),
            b"shm"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏归档要明确报错，绝不能当成「空的」而静默跳过还原。
    #[test]
    fn garbage_archive_is_error() {
        let dir = tmp_dir("garbage");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let bad = dir.join("bad.zip");
        std::fs::write(&bad, b"this is definitely not a zip file").unwrap();

        assert!(restore_backup(&bad, &root).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 归档里不认识的条目应被忽略：不写入、也不报错。
    #[test]
    fn unknown_entries_are_ignored_on_restore() {
        let dir = tmp_dir("unknown");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();

        let bytes = build_stored_zip(&[
            ("voice2word.db".to_string(), b"db".to_vec()),
            ("README.md".to_string(), b"not ours".to_vec()),
        ])
        .unwrap();
        let archive = dir.join("snap.zip");
        std::fs::write(&archive, bytes).unwrap();

        let outcome = restore_backup(&archive, &root).unwrap();
        assert_eq!(outcome.restored, vec!["voice2word.db"]);
        assert!(!root.join("README.md").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CRC-32 已知向量：手写实现必须和标准一致，否则写出的 zip 会被解压器判为损坏。
    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    /// 同一秒内的第二次备份绝不能覆盖第一份：第二份自动用 `-1` 后缀另存，
    /// 两份归档都要在、且各自装着备份当时的库内容（修前第二次会静默覆盖第一次，
    /// 用户以为有两份快照、实际只剩一份）。
    #[test]
    fn same_second_backup_does_not_overwrite_existing_archive() {
        let dir = tmp_dir("collision");
        let root = dir.join("root");
        make_db(&root.join("voice2word.db"), &["first"]);

        // 固定时间戳：模拟「同一秒里连点两次备份」产生的同名 dest。
        let fixed = chrono::Local
            .with_ymd_and_hms(2026, 10, 9, 8, 7, 6)
            .unwrap();
        let dest = dir.join("backups").join(backup_file_name(fixed));

        let first = create_backup(&root, &dest).unwrap();
        assert_eq!(first.archive, dest);

        let live = rusqlite::Connection::open(root.join("voice2word.db")).unwrap();
        live.execute("UPDATE tasks SET note = 'second'", [])
            .unwrap();
        drop(live);
        let second = create_backup(&root, &dest).unwrap();
        assert_ne!(
            second.archive, dest,
            "同名归档必须改用派生文件名，不能原地覆盖"
        );
        assert!(dest.exists(), "第一份归档必须原样保留");
        assert!(second.archive.exists(), "第二份归档必须落盘");

        let notes_of = |archive: &std::path::Path| {
            let inspect = dir.join(format!(
                "inspect-{}.db",
                archive.file_name().unwrap().to_string_lossy()
            ));
            std::fs::write(&inspect, archive_entry(archive, "voice2word.db")).unwrap();
            db_notes(&inspect)
        };
        assert_eq!(notes_of(&dest), vec!["first"]);
        assert_eq!(notes_of(&second.archive), vec!["second"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **核心反例**：当前目录里的主库还活着（且已 checkpoint，没有 `-wal`），
    /// 旧版本归档里带着同一份旧快照的 `-wal`。还原必须**整组**放弃——绝不能把旧
    /// 快照的 WAL 写进当前目录，那会让 SQLite 下次启动拿它回放当前主库。
    ///
    /// 新归档已不再带伴随文件，但旧归档仍可能流落在用户手里，这条保护必须保留。
    /// 逐个文件判断的旧实现会在这里失败：`voice2word.db` 被跳过，而
    /// `voice2word.db-wal` 因为目标不存在而被写入，界面还报「已恢复 1 个文件」。
    #[test]
    fn restore_never_writes_stale_wal_beside_live_db() {
        let dir = tmp_dir("wal_mix");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();

        // 旧版本产出的归档：主库 + 同一份快照的 -wal
        let archive = dir.join("snap.zip");
        let bytes = build_stored_zip(&[
            ("voice2word.db".to_string(), b"old-db".to_vec()),
            ("voice2word.db-wal".to_string(), b"old-wal".to_vec()),
        ])
        .unwrap();
        std::fs::write(&archive, bytes).unwrap();

        // 归档之后用户继续干活：当前目录里的库还在，且已 checkpoint（没有 -wal）。
        std::fs::write(root.join("voice2word.db"), b"LIVE-CURRENT-DATA").unwrap();

        let outcome = restore_backup(&archive, &root).unwrap();
        assert!(outcome.restored.is_empty(), "主库已存在时整组都不能还原");
        assert!(
            !root.join("voice2word.db-wal").exists(),
            "绝不能把旧快照的 WAL 写到当前主库旁边"
        );
        assert_eq!(
            std::fs::read(root.join("voice2word.db")).unwrap(),
            b"LIVE-CURRENT-DATA"
        );
        assert_eq!(
            outcome.skipped_existing,
            vec!["voice2word.db".to_string(), "voice2word.db-wal".to_string()],
            "整组被跳过的条目都要回报给界面，不能有静默部分还原"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 反例的另一半：目标目录里只有**孤立的** `-wal`（没有主库）时同样整组跳过。
    /// 把归档里的主库写下去、旁边却躺着另一份快照的 WAL，一样是「混合两份快照」。
    #[test]
    fn restore_skips_group_when_only_companion_exists() {
        let dir = tmp_dir("wal_only");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();

        // 旧版本产出的归档（带 -wal）
        let archive = dir.join("snap.zip");
        let bytes = build_stored_zip(&[
            ("voice2word.db".to_string(), b"old-db".to_vec()),
            ("voice2word.db-wal".to_string(), b"old-wal".to_vec()),
        ])
        .unwrap();
        std::fs::write(&archive, bytes).unwrap();

        // 主库没了、只剩下没人认领的 WAL（手工删库 / 半途中断都会留下这种残局）。
        std::fs::write(root.join("voice2word.db-wal"), b"old-wal").unwrap();

        let outcome = restore_backup(&archive, &root).unwrap();
        assert!(outcome.restored.is_empty(), "同组里有残留就不能还原");
        assert!(
            !root.join("voice2word.db").exists(),
            "不能把归档里的主库写到残留的 WAL 旁边"
        );
        assert_eq!(
            std::fs::read(root.join("voice2word.db-wal")).unwrap(),
            b"old-wal",
            "残留文件必须原样保留"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正向面：全新目录（换机器）里必须把整组都还原出来——组保护不能把正常场景
    /// 一起挡掉（用旧版本归档验证，新归档里只有单文件快照）。
    #[test]
    fn restore_writes_whole_group_on_fresh_root() {
        let dir = tmp_dir("fresh_group");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();

        let archive = dir.join("snap.zip");
        let bytes = build_stored_zip(&[
            ("voice2word.db".to_string(), b"db-bytes".to_vec()),
            ("voice2word.db-wal".to_string(), b"wal-bytes".to_vec()),
        ])
        .unwrap();
        std::fs::write(&archive, bytes).unwrap();

        let fresh = dir.join("fresh");
        let outcome = restore_backup(&archive, &fresh).unwrap();
        assert_eq!(
            outcome.restored,
            vec!["voice2word.db".to_string(), "voice2word.db-wal".to_string()]
        );
        assert!(outcome.skipped_existing.is_empty());
        assert_eq!(
            std::fs::read(fresh.join("voice2word.db")).unwrap(),
            b"db-bytes"
        );
        assert_eq!(
            std::fs::read(fresh.join("voice2word.db-wal")).unwrap(),
            b"wal-bytes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归档尺寸 / 条目数超过 zip 格式上限时必须报错，而不是让 `as u32` 回绕出一个
    /// 「写得出来、但中央目录指向乱处」的坏归档。
    #[test]
    fn zip_limits_reject_over_4g_or_too_many_entries() {
        assert!(check_zip_limits(0, 1).is_ok());
        assert!(check_zip_limits(u32::MAX as usize, u16::MAX as usize).is_ok());
        // 4 GiB + 1 字节：偏移会回绕
        assert!(check_zip_limits(u32::MAX as usize + 1, 1).is_err());
        // 65536 个条目：u16 计数会回绕
        assert!(check_zip_limits(16, u16::MAX as usize + 1).is_err());
    }
}
