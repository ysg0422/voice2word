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
//! 本模块因此产出**一个带时间戳的归档**，并且把同名的 `-wal` / `-shm` 一并打进去，
//! 用户可以把它当作一个文件保存 / 搬运 / 回退。
//!
//! # 职责边界
//!
//! 这里**只做文件搬运**：读文件、写 zip、再写回文件。不认识「任务」是什么，
//! 不碰 SQLite API，不弹界面。好处是这一层可以被纯单测覆盖（临时目录里造几个
//! 文件就能验证全部行为），而界面 / 业务层只需调用 [`create_backup`] 与
//! [`restore_backup`] 并展示返回结构。
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

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

/// 备份里包含的文件（相对项目根）。顺序即写入归档的顺序。
pub const BACKUP_ENTRIES: [&str; 3] = ["voice2word.db", "config.toml", "config.local.toml"];

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
/// - `root`：项目根（文件从 `root` 下按 [`BACKUP_ENTRIES`] 取）。
/// - `dest`：归档落点（例如 `root/backups/voice2word-backup-<时间戳>.zip`）；
///   父目录不存在会自动创建。
/// - 不存在的条目**跳过并记日志**，不算失败（瘦包里可能还没有 `config.local.toml`）；
///   但**一个都没有**时返回 `Err`（空归档没有任何意义，静默产出一个 0 条目的 zip
///   会让用户以为备份成功了）。
/// - 同一路径的 `-wal` / `-shm` 若存在也一并打进归档：SQLite 的 WAL 模式下
///   主库文件可能还没吸收最近的写入，只备份 `.db` 会丢最后几次编辑。
pub fn create_backup(root: &Path, dest: &Path) -> Result<BackupOutcome> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建备份目录失败: {}", parent.display()))?;
        }
    }

    // 先收集 (归档内名字, 文件字节)。注意顺序：先 BACKUP_ENTRIES 本体，
    // 每个本体后面紧跟它的 -wal / -shm 伴随文件（如果存在）。
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut included: Vec<String> = Vec::new();

    for name in BACKUP_ENTRIES {
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
        // WAL 模式下 -wal 里可能压着最近未合并的写入，-shm 是配套的索引文件。
        // 两者都跟着主库走，否则还原出来的库可能缺最后一次编辑。
        for suffix in ["-wal", "-shm"] {
            let companion = root.join(format!("{name}{suffix}"));
            if companion.is_file() {
                let bytes = std::fs::read(&companion)
                    .with_context(|| format!("读取备份伴随文件失败: {}", companion.display()))?;
                entries.push((format!("{name}{suffix}"), bytes));
                included.push(format!("{name}{suffix}"));
            }
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
    std::fs::write(dest, &bytes).with_context(|| format!("写入归档失败: {}", dest.display()))?;

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

/// 从归档恢复：把归档里的条目写回 `root`。
///
/// - **不覆盖已有文件**：目标已存在时**不**写入，并把它记进
///   [`RestoreOutcome::skipped_existing`] 返回给调用方（界面据此提示
///   「这些文件已存在，已保留现有版本」）。理由：覆盖 `voice2word.db` 会毁掉
///   用户当前的全部工程，这种操作必须由用户显式先移走/重命名旧文件，
///   不能由一次误点触发。
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

    for (name, data) in &files {
        // 只认自己认识的条目：归档里多出来的东西（未来版本 / 手工塞的）一律忽略，
        // 既避免往项目根里写陌生文件，也避免旧版本读到新格式时误解。
        if !is_known_entry(name) {
            continue;
        }
        let target = root.join(name);
        if target.exists() {
            // 关键安全阀：已存在就保留现有版本，绝不覆盖用户当前的库。
            warn!(target = %target.display(), "还原目标已存在，保留现有版本");
            skipped_existing.push(name.clone());
            continue;
        }
        std::fs::write(&target, data)
            .with_context(|| format!("写入还原文件失败: {}", target.display()))?;
        restored.push(name.clone());
    }

    Ok(RestoreOutcome {
        restored,
        skipped_existing,
    })
}

/// 备份文件名：`voice2word-backup-YYYYMMDD-HHMMSS.zip`（本地时间）。
/// 抽成纯函数便于单测（不依赖时钟）。
pub fn backup_file_name(now: chrono::DateTime<chrono::Local>) -> String {
    now.format("voice2word-backup-%Y%m%d-%H%M%S.zip")
        .to_string()
}

/// 条目名是否属于本模块认识的范围（本体或它的 -wal/-shm 伴随文件）。
fn is_known_entry(name: &str) -> bool {
    BACKUP_ENTRIES
        .iter()
        .any(|base| name == *base || name == format!("{base}-wal") || name == format!("{base}-shm"))
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

    /// 往返：备份 → 清空 root → 还原，字节必须完全一致。
    /// 这是本模块存在的意义：换机器 / 回退后数据不能有任何偏差。
    #[test]
    fn round_trip_restores_bytes() {
        let dir = tmp_dir("roundtrip");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("voice2word.db"),
            b"SQLite format 3\0fake db bytes",
        )
        .unwrap();
        std::fs::write(root.join("config.toml"), b"language = \"zh\"\n").unwrap();

        let archive = dir.join("backups").join("snap.zip");
        let outcome = create_backup(&root, &archive).unwrap();
        assert_eq!(outcome.archive, archive);
        assert_eq!(outcome.included, vec!["voice2word.db", "config.toml"]);
        assert!(outcome.bytes > 0);

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir_all(&root).unwrap();

        let restored = restore_backup(&archive, &root).unwrap();
        assert_eq!(restored.restored, vec!["voice2word.db", "config.toml"]);
        assert!(restored.skipped_existing.is_empty());
        assert_eq!(
            std::fs::read(root.join("voice2word.db")).unwrap(),
            b"SQLite format 3\0fake db bytes"
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
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("voice2word.db"), b"db").unwrap();

        let archive = dir.join("snap.zip");
        let outcome = create_backup(&root, &archive).unwrap();
        assert!(outcome.included.contains(&"voice2word.db".to_string()));
        assert!(!outcome.included.contains(&"config.local.toml".to_string()));
        assert!(archive.is_file());

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
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("voice2word.db"), b"OLD-BACKUP-CONTENT").unwrap();

        let archive = dir.join("snap.zip");
        create_backup(&root, &archive).unwrap();

        // 用户在当前工程里继续干活，主库已经变了。
        std::fs::write(root.join("voice2word.db"), b"LIVE-CURRENT-DATA").unwrap();

        let outcome = restore_backup(&archive, &root).unwrap();
        assert!(outcome.restored.is_empty());
        assert_eq!(outcome.skipped_existing, vec!["voice2word.db"]);
        assert_eq!(
            std::fs::read(root.join("voice2word.db")).unwrap(),
            b"LIVE-CURRENT-DATA"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WAL 模式下的伴随文件必须一起进归档，否则还原出来的库可能缺最后一次编辑。
    #[test]
    fn wal_and_shm_companions_are_included() {
        let dir = tmp_dir("wal");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("voice2word.db"), b"db").unwrap();
        std::fs::write(root.join("voice2word.db-wal"), b"wal-bytes").unwrap();
        std::fs::write(root.join("voice2word.db-shm"), b"shm").unwrap();

        let archive = dir.join("snap.zip");
        let outcome = create_backup(&root, &archive).unwrap();
        assert_eq!(
            outcome.included,
            vec!["voice2word.db", "voice2word.db-wal", "voice2word.db-shm"]
        );

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let restored = restore_backup(&archive, &root).unwrap();
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
}
