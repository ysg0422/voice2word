//! 最小 ZIP 解压器（仅够本项目的组件下载用）。
//!
//! # 为什么不引入 `zip` crate
//!
//! 组件的镜像分发是 **`.zip`**（llama.cpp 官方 Windows 构建就是打好的 zip），
//! 而本仓库依赖树里没有可用的 ZIP 解压库（`zip` crate 不在离线缓存里，crates.io
//! 也不可达）。`flate2` 已在依赖中（用于其它用途），因此这里直接按 ZIP 规范实现
//! 一个**只读**解压器：解析中央目录 → 逐个条目读本地头 → 按方法解压写盘。
//!
//! # 支持范围
//!
//! - 压缩方法：**存储(0)** 与 **deflate(8)**——llama.cpp 的官方 zip 只用这两种。
//! - 不做 Zip64、加密、多卷；遇到这些（或超 4GB）一律返回错误而不是猜。
//! - 拒绝路径穿越（含 `..`、绝对路径、盘符）的条目名，只写普通文件名。
//!   同时拒绝 Win32 保留设备名（`NUL` / `CON` …）、名字里带 `:` 的 NTFS 数据流、
//!   以及会被系统改名（尾随点 / 空格）的形状——它们要么让文件被静默丢弃，
//!   要么让落盘名与判定名不一致（见 [`is_safe_name`]）。
//! - 若整个包被套在一个顶层目录下（HuggingFace 仓库打包很常见），
//!   解压时自动剥掉这层前缀，文件直接落在 `dest_dir`。
//!
//! 目标是「够用且不会错」，不是通用 ZIP 库。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

/// 中央目录文件头签名 `PK\x01\x02`
const SIG_CENTRAL: u32 = 0x0201_4b50;
/// 本地文件头签名 `PK\x03\x04`
const SIG_LOCAL: u32 = 0x0403_4b50;
/// 中央目录结束记录签名 `PK\x05\x06`
const SIG_EOCD: u32 = 0x0605_4b50;

fn read_u16(b: &[u8], off: usize) -> Result<u16> {
    let s = b
        .get(off..off + 2)
        .ok_or_else(|| anyhow!("zip 数据越界（读 u16 @{off}）"))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32(b: &[u8], off: usize) -> Result<u32> {
    let s = b
        .get(off..off + 4)
        .ok_or_else(|| anyhow!("zip 数据越界（读 u32 @{off}）"))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// 定位中央目录结束记录（EOCD）。注释最长 65535，故从尾部向前找签名。
fn find_eocd(b: &[u8]) -> Result<usize> {
    if b.len() < 22 {
        bail!("不是合法的 zip（长度不足 EOCD）");
    }
    let min = b.len().saturating_sub(22 + 0xffff);
    let mut i = b.len() - 22;
    loop {
        if read_u32(b, i).map(|v| v == SIG_EOCD).unwrap_or(false) {
            return Ok(i);
        }
        if i == min {
            break;
        }
        i -= 1;
    }
    bail!("未找到 zip 中央目录结束记录（文件可能损坏或截断）")
}

/// 条目名是否安全（防路径穿越）。只允许普通文件名，禁止目录层级与盘符。
///
/// 注意：调用方会先剥掉顶层包裹目录，因此到这里时名字已是相对于目标目录的。
fn is_safe_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // 目录条目（以 / 结尾）跳过，不当作文件写
    if name.ends_with('/') || name.ends_with('\\') {
        return false;
    }
    // NUL 截断类：`a\0.txt` 在部分下游 API 里会被截成 `a`，行为不可预期，直接拒绝。
    if name.contains('\0') {
        return false;
    }
    // `..` 出现在任何位置都拒绝（`a..b` 这种无害名也一并拒掉，换取规则简单可靠）
    if name.contains("..") {
        return false;
    }
    // 禁止绝对路径与盘符
    if name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    // 任何位置的 `:` 一律拒绝，而不是只看「第二个字符是不是盘符」：
    // `ab:stream.exe` 会被 Win32 解释成 NTFS 备用数据流（ADS），内容落进 `ab` 的
    // 隐藏流里——`exists()` 为真、目录列表里却看不到，等于绕过了「只在目标目录里写
    // 扁平常规文件」这条设计，写出的东西用户既看不见也删不干净。
    if name.contains(':') {
        return false;
    }
    // 尾随的点 / 空格在 Windows 上会被归一化掉：`a.dll.` 实际落盘成 `a.dll`，
    // 于是「判定用的名字」和「真实文件」不是同一个，两个条目会互相覆盖，
    // 覆盖与否还取决于中央目录顺序。直接拒绝这种会被改名的形状。
    if name.ends_with('.') || name.ends_with(' ') {
        return false;
    }
    // Windows 保留设备名：见 [`RESERVED_DEVICE_NAMES`]。
    if is_reserved_device_name(name) {
        return false;
    }
    // 禁止内嵌目录分隔（llama.cpp 的 zip 是扁平结构，内嵌目录一律拒绝，
    // 既简化实现又避免「写到子目录里」这种没被校验覆盖的情况）
    !name.contains('/') && !name.contains('\\')
}

/// Windows 保留设备名（大小写不敏感）。
///
/// 为什么必须拒绝：这些名字在 Win32 路径解析里指向**设备**而不是文件，后果
/// 与「普通坏名」不同——
/// - 裸 `NUL` / `nul`：`File::create` 与 `write_all` 都返回 `Ok`，但磁盘上
///   **没有文件**、`exists()` 为假 → 条目被静默丢弃，写出的文件数还被虚报。
///   丢的若是入口文件，调用方只会报「解压完成但缺少入口文件」，把人带偏。
/// - `CON` / `AUX` / `COM1`…：在部分 Windows 版本上打开会阻塞或直接失败。
///
/// 组件压缩包里不可能出现这些名字，拒绝它们不会误伤正常下载。
const RESERVED_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 名字（去掉扩展名，再去掉尾随点/空格）是否命中保留设备名。
fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let stem = stem.trim_end_matches([' ', '.']);
    RESERVED_DEVICE_NAMES
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(device))
}

/// 单个条目解压后的**硬上限**。中央目录声明的 `uncomp_size` 是攻击者可控的
/// 字段，不可信；因此按「声明值留出余量」再解压，多余字节直接拒绝，
/// 避免一个几十 KB 的 deflate 包解出几十 GB 把内存/磁盘打满。
///
/// 本项目最大组件（llama.cpp 全量包）解压后约 500 MB，1 GB 上限足够宽松。
const MAX_ENTRY_UNCOMPRESSED: u64 = 1024 * 1024 * 1024;
/// 所有条目解压后的累计上限（同一个包可能塞很多条）。
const MAX_TOTAL_UNCOMPRESSED: u64 = 2 * 1024 * 1024 * 1024;

/// 把 zip 字节解压到 `dest_dir`，返回写出的文件数。
///
/// 同名文件直接覆盖（组件升级场景）。任何一步失败都返回错误，由调用方决定
/// 是否回滚（本项目里 `.part` 与大小校验已能拦住截断下载）。
///
/// # 包一层目录的处理
///
/// 很多上游 zip（尤其 HuggingFace 仓库打包）会把所有文件套在一个
/// 顶层目录里（如 `llama-b9637-bin-win-cpu-x64/llama.dll`）。这种情况下直接拒绝
/// 所有条目会让整个下载失败（用户只看到「解压失败」，不知道原因）。
/// 因此：若**全部**有效条目都在同一个顶层目录下，就把这层前缀剥掉后再落盘；
/// 否则任何带目录层级的条目一律跳过（不碰）。即便剥前缀后，最终文件名仍必须通过
/// `is_safe_name`——绝不允许写到 `dest_dir` 之外。
pub fn extract_zip(bytes: &[u8], dest_dir: &Path) -> Result<usize> {
    let entries = parse_central_directory(bytes)?;
    if entries.is_empty() {
        bail!("压缩包里没有任何条目");
    }
    let strip = common_prefix(&entries);

    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("创建目录失败: {}", dest_dir.display()))?;

    let mut written = 0usize;
    let mut total_out: u64 = 0;
    for e in &entries {
        let name = strip
            .as_deref()
            .and_then(|p| e.name.strip_prefix(p))
            .unwrap_or(e.name.as_str());
        if !is_safe_name(name) {
            continue;
        }
        let data = read_entry_data(bytes, e)?;
        // 声明值不可信，只用来给 `Vec` 预留容量（并设上限，避免预留本身就把内存打满）。
        let per_entry_cap = entry_size_cap(e.uncomp_size)?;
        let mut out = Vec::with_capacity((e.uncomp_size as usize).min(1 << 20));
        match e.method {
            0 => {
                // 存储法没有「压缩比」可爆炸，但声明的未压缩长度与实际字节数不一致
                // 说明包结构已损坏：直接拒绝，避免下游按错误长度解读文件。
                if data.len() as u64 != u64::from(e.uncomp_size) {
                    bail!(
                        "存储条目长度不一致: {}（声明 {} 字节，实际 {} 字节）",
                        name,
                        e.uncomp_size,
                        data.len()
                    );
                }
                out.extend_from_slice(data);
            }
            8 => {
                // 多读 1 字节：能读出第 cap+1 个字节就说明实际内容超过上限，
                // 据此判定解压炸弹，而不是等到内存耗尽。
                let mut dec = flate2::read::DeflateDecoder::new(data).take(per_entry_cap + 1);
                dec.read_to_end(&mut out)
                    .with_context(|| format!("解压 deflate 条目失败: {name}"))?;
            }
            other => bail!("不支持的压缩方法 {other}: {name}"),
        }
        if out.len() as u64 > per_entry_cap {
            bail!(
                "解压后体积超出预期（条目 {} 声明 {} 字节，实际已超上限 {} 字节），\
                 疑似损坏的压缩包或解压炸弹",
                name,
                e.uncomp_size,
                per_entry_cap
            );
        }
        // deflate 条目：解压长度必须与声明一致。`FlateDecoder` 在数据被截断时
        // 会返回错误（已在上面 `?` 掉），但「声明 1 MB、实际只解出 1 字节」这种
        // 内部不一致的包仍要拦住——落盘一个残缺的 DLL 比报错更难排查。
        if e.method == 8 && out.len() as u64 != u64::from(e.uncomp_size) {
            bail!(
                "解压条目长度与声明不符: {}（声明 {} 字节，实际 {} 字节）",
                name,
                e.uncomp_size,
                out.len()
            );
        }
        // CRC32 校验：只有「长度对、内容错」的损坏（传输位翻转、镜像返回被截断又补齐的包）
        // 会被长度检查漏掉，落到磁盘上就是**静默损坏的 DLL**——加载时报的错会离谱到
        // 完全指不回这里。上游发布的 zip 都带真实 CRC，所以这一条能真正兜住。
        if e.crc32 != 0 {
            let actual = crc32_ieee(&out);
            if actual != e.crc32 {
                bail!(
                    "解压条目 CRC32 校验失败: {}（声明 {:08x}，实际 {:08x}）——压缩包可能已损坏",
                    name,
                    e.crc32,
                    actual
                );
            }
        }
        total_out = total_out.saturating_add(out.len() as u64);
        if total_out > MAX_TOTAL_UNCOMPRESSED {
            bail!("解压总输出超过 {MAX_TOTAL_UNCOMPRESSED} 字节上限，已中止");
        }

        let target: PathBuf = dest_dir.join(name);
        let mut f = std::fs::File::create(&target)
            .with_context(|| format!("创建文件失败: {}", target.display()))?;
        f.write_all(&out)
            .with_context(|| format!("写入文件失败: {}", target.display()))?;
        written += 1;
    }
    Ok(written)
}

/// 单个条目允许解压出的最大字节数：以中央目录的声明值为准，但留出余量，
/// 并夹在全局上限内。声明值本身就是畸形（大到离谱）时直接拒绝。
fn entry_size_cap(declared: u32) -> Result<u64> {
    let declared = u64::from(declared);
    if declared > MAX_ENTRY_UNCOMPRESSED {
        bail!("条目声明的解压体积过大: {declared} 字节");
    }
    Ok(declared
        .saturating_add(64 * 1024)
        .min(MAX_ENTRY_UNCOMPRESSED))
}

/// 一个待解压的条目（已从中央目录解析出来）。
struct ZipEntry {
    /// 原始条目名（未剥前缀）
    name: String,
    method: u16,
    comp_size: u32,
    uncomp_size: u32,
    local_off: u32,
    /// 中央目录里声明的 CRC32。`0` 视为「未提供」（本仓库自己的备份 zip 就写 0），
    /// 此时跳过校验；上游发布的包都会带真实的 CRC。
    crc32: u32,
}

/// 解析中央目录，返回全部条目。
///
/// 为什么要先解析完才写盘：需要先知道全部条目名才能判断
/// 「是否都在同一个顶层目录下」，进而决定要不要剥前缀。
fn parse_central_directory(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    let eocd = find_eocd(bytes)?;
    let total = read_u16(bytes, eocd + 10)? as usize;
    let cd_off = read_u32(bytes, eocd + 16)? as usize;
    let cd_size = read_u32(bytes, eocd + 12)? as usize;
    if cd_off == 0xffff_ffff || cd_size == 0xffff_ffff || total == 0xffff {
        bail!("不支持 Zip64 格式的压缩包");
    }
    if cd_off
        .checked_add(cd_size)
        .map(|end| end > bytes.len())
        .unwrap_or(true)
    {
        bail!("zip 中央目录越界（文件截断）");
    }

    let mut entries = Vec::with_capacity(total);
    let mut p = cd_off;
    for _ in 0..total {
        if read_u32(bytes, p)? != SIG_CENTRAL {
            bail!("中央目录条目签名不正确（偏移 {p}）");
        }
        let method = read_u16(bytes, p + 10)?;
        let crc32 = read_u32(bytes, p + 16)?;
        let comp_size = read_u32(bytes, p + 20)?;
        let uncomp_size = read_u32(bytes, p + 24)?;
        let name_len = read_u16(bytes, p + 28)? as usize;
        let extra_len = read_u16(bytes, p + 30)? as usize;
        let comment_len = read_u16(bytes, p + 32)? as usize;
        let local_off = read_u32(bytes, p + 42)?;
        let name = String::from_utf8_lossy(
            bytes
                .get(p + 46..p + 46 + name_len)
                .ok_or_else(|| anyhow!("条目名越界"))?,
        )
        .into_owned();
        p += 46 + name_len + extra_len + comment_len;

        if comp_size == 0xffff_ffff || uncomp_size == 0xffff_ffff || local_off == 0xffff_ffff {
            bail!("不支持 Zip64 条目: {name}");
        }
        entries.push(ZipEntry {
            name,
            method,
            comp_size,
            uncomp_size,
            local_off,
            crc32,
        });
    }
    Ok(entries)
}

/// 若全部非目录条目共用同一个顶层目录前缀，返回该前缀（带尾部 `/`）。
///
/// 这里只考虑**单一共同**前缀：混合结构（部分在根、部分在子目录）不剥，
/// 交给 `is_safe_name` 逐条判定——宁可少写几个文件，也不能猜错结构。
fn common_prefix(entries: &[ZipEntry]) -> Option<String> {
    let mut prefix: Option<&str> = None;
    let mut any_file = false;
    for e in entries {
        // 目录条目（以 / 结尾）不参与判定
        if e.name.ends_with('/') || e.name.ends_with('\\') {
            continue;
        }
        any_file = true;
        let Some((first, _)) = e.name.split_once('/') else {
            // 有直接落在根下的文件 → 没有可剥的共同前缀
            return None;
        };
        match prefix {
            None => prefix = Some(first),
            Some(p) if p == first => {}
            Some(_) => return None,
        }
    }
    if !any_file {
        return None;
    }
    // 前缀不能含盘符等危险字符（虽然剥掉后不会写出，但保持严谨）
    let p = prefix?;
    if p.is_empty() || p.contains('\\') || p.contains(':') {
        return None;
    }
    Some(format!("{p}/"))
}

/// 标准 CRC-32（IEEE 802.3，与 zip 用的多项式一致）。
///
/// 自己实现而不是引 `crc32fast`：本模块的整体设计就是「离线可构建、不引新依赖」
/// （zip 解析本身也是手写的）。表驱动实现足够快——解压是 IO 主导，CRC 只占很小的比例。
fn crc32_ieee(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}
/// 从已解析的条目中取出压缩数据（按本地头算出数据起始偏移）。
fn read_entry_data<'a>(bytes: &'a [u8], e: &ZipEntry) -> Result<&'a [u8]> {
    let local_off = e.local_off as usize;
    if read_u32(bytes, local_off)? != SIG_LOCAL {
        bail!("本地文件头签名不正确: {}", e.name);
    }
    let l_name_len = read_u16(bytes, local_off + 26)? as usize;
    let l_extra_len = read_u16(bytes, local_off + 28)? as usize;
    let data_start = local_off + 30 + l_name_len + l_extra_len;
    bytes
        .get(data_start..data_start + e.comp_size as usize)
        .ok_or_else(|| anyhow!("条目数据越界: {}", e.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC32 必须与已知向量一致（`123456789` → `0xCBF43926`），否则校验会全部失败。
    #[test]
    fn crc32_matches_known_vector() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);
    }

    /// 内容被改坏但长度不变时，必须靠 CRC 拦住——这正是不校验时最危险的情形：
    /// 落盘一个「长度正确、内容损坏」的 DLL，加载时报的错完全指不回压缩包。
    #[test]
    fn rejects_entry_whose_crc_does_not_match() {
        let dir = std::env::temp_dir().join(format!("v2w_crc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let payload = b"hello world".to_vec();
        // 内容正确、但中央目录里声明的 CRC 是错的 —— 模拟传输位翻转。
        let bad_crc = crc32_ieee(&payload) ^ 0xFFFF_FFFF;
        let zip = build_stored_zip_declaring_crc("a.txt", &payload, bad_crc);

        let err = extract_zip(&zip, &dir).expect_err("CRC 不符必须拒绝");
        let msg = format!("{err:#}");
        assert!(msg.contains("CRC32"), "错误文案应点名 CRC：{msg}");
        assert!(
            !dir.join("a.txt").exists(),
            "校验失败的条目不得落盘（否则就是静默损坏）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 构造一个单条目（存储法）zip，并**按调用方给的 CRC** 写进本地头与中央目录。
    ///
    /// 与 `build_stored_zip` 的区别就这一点：那个一律写 0（= 未提供，跳过校验），
    /// 这个用来构造「声明了 CRC」的包，好验证校验逻辑本身。
    fn build_stored_zip_declaring_crc(name: &str, data: &[u8], crc: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        let local_off = 0u32;
        out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // stored
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&local_off.to_le_bytes());
        central.extend_from_slice(name.as_bytes());

        let cd_off = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// 手工构造一个最小 zip：n 个「存储」条目。用来验证解析与写盘。    /// 手工构造一个最小 zip：n 个「存储」条目。用来验证解析与写盘。    /// 手工构造一个最小 zip：n 个「存储」条目。用来验证解析与写盘。
    fn build_stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let local_off = out.len() as u32;
            // 本地头
            out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method = stored
            out.extend_from_slice(&0u16.to_le_bytes()); // mod time
            out.extend_from_slice(&0u16.to_le_bytes()); // mod date
            out.extend_from_slice(&0u32.to_le_bytes()); // crc (not checked)
            out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // comp size
            out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncomp size
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            // 中央目录条目
            central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&0u16.to_le_bytes()); // flags
            central.extend_from_slice(&0u16.to_le_bytes()); // method
            central.extend_from_slice(&0u16.to_le_bytes()); // time
            central.extend_from_slice(&0u16.to_le_bytes()); // date
            central.extend_from_slice(&0u32.to_le_bytes()); // crc
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attr
            central.extend_from_slice(&0u32.to_le_bytes()); // external attr
            central.extend_from_slice(&local_off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        // EOCD
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    /// 用 flate2 压缩数据，构造一个 deflate 方法的 zip（复用上面的中央目录写法）。
    fn build_deflate_zip(name: &str, raw: &[u8]) -> Vec<u8> {
        let mut comp = Vec::new();
        {
            let mut enc =
                flate2::write::DeflateEncoder::new(&mut comp, flate2::Compression::default());
            enc.write_all(raw).unwrap();
            enc.finish().unwrap();
        }
        let mut out = Vec::new();
        let mut central = Vec::new();
        let local_off = 0u32;
        out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes()); // method = deflate
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&comp);

        central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&8u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        central.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&local_off.to_le_bytes());
        central.extend_from_slice(name.as_bytes());

        let cd_off = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("v2w_zip_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn extracts_stored_entries() {
        let zip = build_stored_zip(&[("a.exe", b"hello"), ("b.dll", b"world!")]);
        let dir = tmp_dir("stored");
        let n = extract_zip(&zip, &dir).unwrap();
        assert_eq!(n, 2);
        assert_eq!(std::fs::read(dir.join("a.exe")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dir.join("b.dll")).unwrap(), b"world!");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_deflate_entry() {
        let raw = vec![b'x'; 4096];
        let zip = build_deflate_zip("big.bin", &raw);
        let dir = tmp_dir("deflate");
        assert_eq!(extract_zip(&zip, &dir).unwrap(), 1);
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), raw);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_truncated_and_garbage() {
        assert!(extract_zip(b"not a zip", &tmp_dir("garbage")).is_err());
        let zip = build_stored_zip(&[("a.exe", b"hello")]);
        let truncated = &zip[..zip.len() - 8];
        assert!(extract_zip(truncated, &tmp_dir("trunc")).is_err());
    }

    #[test]
    fn skips_path_traversal_names() {
        // 条目名含 .. 时必须被跳过，绝不写到 dest_dir 之外
        let zip = build_stored_zip(&[("../evil.exe", b"x"), ("ok.exe", b"y")]);
        let dir = tmp_dir("trav");
        let n = extract_zip(&zip, &dir).unwrap();
        assert_eq!(n, 1, "只应写出安全条目");
        assert!(dir.join("ok.exe").exists());
        assert!(!dir.parent().unwrap().join("evil.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strips_single_top_level_wrapper_dir() {
        // HuggingFace 仓库打包很常见：所有文件套在一个顶层目录里。
        // 应该剥掉这层前缀，让 llama-completion.exe 直接落在 dest_dir。
        let zip = build_stored_zip(&[
            ("llama-b9637/llama-completion.exe", b"exe"),
            ("llama-b9637/llama-server.exe", b"server"),
            ("llama-b9637/llama.dll", b"dll"),
        ]);
        let dir = tmp_dir("wrapped");
        let n = extract_zip(&zip, &dir).unwrap();
        assert_eq!(n, 3);
        assert!(dir.join("llama-completion.exe").exists(), "应剥掉顶层目录");
        assert!(dir.join("llama-server.exe").exists());
        assert!(!dir.join("llama-b9637").exists(), "不应留下包裹目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_mixed_layout_without_stripping() {
        // 根目录与子目录混合时不剥前缀：根下的文件正常落盘，
        // 子目录里的条目因带目录层级被跳过（宁可少写也不猜）。
        let zip = build_stored_zip(&[("root.exe", b"r"), ("sub/inner.dll", b"i")]);
        let dir = tmp_dir("mixed");
        let n = extract_zip(&zip, &dir).unwrap();
        assert_eq!(n, 1, "只有根下的文件应被写出");
        assert!(dir.join("root.exe").exists());
        assert!(!dir.join("sub").exists(), "不应创建子目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 路径穿越的各种写法都必须被 `is_safe_name` 挡住。
    /// 这些名字一旦漏过一条，解压就会写到目标目录之外。
    #[test]
    fn safe_name_rejects_traversal_and_absolute_forms() {
        // 不安全
        for bad in [
            "",
            "../evil.exe",
            "..\\evil.exe",
            "a/../../evil.exe",
            "sub/../../../evil.exe",
            "/etc/passwd",
            "\\windows\\system32\\evil.dll",
            "C:/Windows/evil.dll",
            "c:evil.dll",
            "sub/inner.dll",  // 内嵌目录
            "sub\\inner.dll", // 内嵌目录（反斜杠）
            "dir/",           // 目录条目
            "dir\\",          // 目录条目
            "a\0b.dll",       // NUL 截断
        ] {
            assert!(!is_safe_name(bad), "应拒绝不安全条目名: {bad:?}");
        }
        // 安全：普通扁平文件名
        for good in ["llama.dll", "whisper-cli.exe", "ggml-base.bin"] {
            assert!(is_safe_name(good), "应接受普通文件名: {good:?}");
        }
    }

    /// 保留设备名 / NTFS 数据流 / 会被系统改名的形状：这些名字不会「写到别处」，
    /// 但会让**文件被静默丢弃**（裸 `NUL`：`File::create` 返回 Ok 却没有文件）
    /// 或**落盘名与判定名不一致**（`a.dll.` → `a.dll`、`ab:stream.exe` → `ab` 的
    /// 隐藏流），同样必须拒绝。
    #[test]
    fn safe_name_rejects_device_ads_and_normalized_forms() {
        for bad in [
            "NUL",     // 裸设备名：写成功但磁盘上没有文件
            "nul",     // 大小写不敏感
            "NUL.txt", // 带扩展名同样是设备名
            "nul.dll", //
            "CON",
            "con", // 控制台设备
            "AUX",
            "PRN", //
            "COM1",
            "com9", // 串口
            "LPT1",
            "lpt1.log",      // 并口
            "ab:stream.exe", // NTFS 备用数据流
            "a.txt:stream",  //
            "ab:",           //
            "a.dll.",        // 尾随点：落盘时被系统吃掉
            "a.dll ",        // 尾随空格
            "trailing.",     //
        ] {
            assert!(
                !is_safe_name(bad),
                "应拒绝会被系统改写/丢弃的条目名: {bad:?}"
            );
        }
        // 正常组件名不能被误伤（含「名字里出现设备名子串」的情况）
        for good in [
            "llama.dll",
            "ggml-cpu.dll",
            "whisper-cli.exe",
            "nulify.dll",
            "console.exe",
            "com10.dll",
            ".gitignore",
        ] {
            assert!(is_safe_name(good), "不应拒绝正常文件名: {good:?}");
        }
    }

    /// 端到端反例：条目名是保留设备名时不许落盘，也不许把它算进「已写出文件数」。
    ///
    /// 旧实现对裸 `NUL` 会走到 `File::create`（返回 `Ok`）、`written += 1`，
    /// 但磁盘上什么都不存在——调用方拿到的「解压成功 1 个文件」是假的。
    #[test]
    fn extract_skips_reserved_device_entry_without_counting_it() {
        let dir = tmp_dir("device_name");
        let zip = build_stored_zip(&[("NUL", b"payload")]);
        assert_eq!(
            extract_zip(&zip, &dir).unwrap(),
            0,
            "保留设备名条目不能被算作已写出"
        );
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(names.is_empty(), "不该落盘任何文件: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端反例：`a.dll.` 与 `a.dll` 在 Windows 上是**同一个文件**。
    /// 不拒绝前者时，后写的条目会覆盖前一个（顺序由中央目录决定），最终用户拿到
    /// 哪个版本不可预期——正常下载的文件可能被同包里一个畸形条目顶掉。
    #[test]
    fn extract_does_not_let_normalized_name_overwrite_real_entry() {
        let dir = tmp_dir("norm_collision");
        let zip = build_stored_zip(&[("a.dll.", b"DECOY"), ("a.dll", b"REAL")]);
        assert_eq!(
            extract_zip(&zip, &dir).unwrap(),
            1,
            "被系统改名的条目不该被写出"
        );
        assert_eq!(
            std::fs::read(dir.join("a.dll")).unwrap(),
            b"REAL",
            "正常条目必须原样落盘"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录穿越条目在真实解压流程里也必须被跳过（端到端，而不只是单测判定函数）。
    #[test]
    fn extract_skips_all_traversal_variants() {
        let zip = build_stored_zip(&[
            ("..\\evil.exe", b"x"),
            ("C:/evil.dll", b"x"),
            ("sub/../evil2.dll", b"x"),
            ("ok.exe", b"y"),
        ]);
        let dir = tmp_dir("trav2");
        let n = extract_zip(&zip, &dir).unwrap();
        assert_eq!(n, 1, "只应写出 ok.exe");
        assert!(dir.join("ok.exe").exists());
        assert!(!dir.parent().unwrap().join("evil.exe").exists());
        assert!(!dir.parent().unwrap().join("evil2.dll").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 解压炸弹：deflate 包声明小体积但实际解出远超声明的内容时，必须报错中止，
    /// 而不是无上限地读进内存。
    #[test]
    fn rejects_decompression_bomb() {
        // 用全零构造一个高度可压缩的大负载（4 MB），但把中央目录里的
        // uncomp_size 谎报成 1 字节——真实解压结果会远超「声明 + 余量」。
        let raw = vec![0u8; 4 * 1024 * 1024];
        let mut zip = build_deflate_zip("bomb.bin", &raw);
        // 找到中央目录条目里的 uncomp_size（+24）改成 1
        let eocd = find_eocd(&zip).unwrap();
        let cd_off = read_u32(&zip, eocd + 16).unwrap() as usize;
        zip[cd_off + 24..cd_off + 28].copy_from_slice(&1u32.to_le_bytes());

        let dir = tmp_dir("bomb");
        let err = extract_zip(&zip, &dir).unwrap_err();
        assert!(
            err.to_string().contains("超出预期") || err.to_string().contains("不符"),
            "应报出解压体积异常: {err}"
        );
        assert!(!dir.join("bomb.bin").exists(), "超限条目不应落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 存储条目声明长度与实际长度不一致时必须报错，而不是按错误长度落盘。
    #[test]
    fn rejects_stored_entry_with_mismatched_declared_size() {
        let mut zip = build_stored_zip(&[("a.exe", b"hello")]);
        // 把中央目录里的 uncomp_size 从 5 改成 999
        let eocd = find_eocd(&zip).unwrap();
        let cd_off = read_u32(&zip, eocd + 16).unwrap() as usize;
        zip[cd_off + 24..cd_off + 28].copy_from_slice(&999u32.to_le_bytes());

        let dir = tmp_dir("mismatch");
        let err = extract_zip(&zip, &dir).unwrap_err();
        assert!(err.to_string().contains("长度不一致"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
