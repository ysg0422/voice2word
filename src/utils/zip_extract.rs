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
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return false;
    }
    // 禁止内嵌目录分隔（llama.cpp 的 zip 是扁平结构，内嵌目录一律拒绝，
    // 既简化实现又避免「写到子目录里」这种没被校验覆盖的情况）
    !name.contains('/') && !name.contains('\\')
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

    /// 手工构造一个最小 zip：n 个「存储」条目。用来验证解析与写盘。
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
