//! 极简 PE 导入表解析（只回答「这份 exe / dll 依赖哪些 DLL」这一个问题）。
//!
//! 为什么需要它：判断一个 whisper-cli 能不能启动，**唯一准确**的依据是它自己
//! 声明的导入表——MinGW 构建导入 `libgcc_s_seh-1.dll` / `libstdc++-6.dll` /
//! `libgomp-1.dll`，MSVC 构建导入 `VCRUNTIME140.dll` / `MSVCP140.dll` /
//! `VCOMP140.dll`，两者互不需要对方的库。若对着一个 MSVC 构建去硬查 MinGW
//! 三件套，就会永远误报「运行库缺失」——这正是把 whisper-cli 换成官方 MSVC
//! 构建后暴露出来的问题。
//!
//! 为什么不用「在文件里搜 DLL 名字字符串」：那会把调试信息、注释、其它字符串
//! 常量里偶然出现的名字也算进去，误报与漏报都不可预期。解析 PE 导入目录是精确的，
//! 且只依赖文件字节，天然可单测。

/// 一个节区的定位信息（RVA → 文件偏移换算用）。
struct Section {
    va: u32,
    vsize: u32,
    raw_size: u32,
    raw_ptr: u32,
}

fn read_u16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// 读一段以 NUL 结尾的 ASCII 字符串（DLL 名）。
fn read_cstr(b: &[u8], off: usize) -> Option<String> {
    let tail = b.get(off..)?;
    let len = tail.iter().position(|&c| c == 0)?;
    std::str::from_utf8(&tail[..len]).ok().map(str::to_string)
}

fn parse_sections(b: &[u8], start: usize, count: usize) -> Option<Vec<Section>> {
    // 节区表理论上不该有上万个；给个上限防御畸形文件。
    if count == 0 || count > 96 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = start.checked_add(i.checked_mul(40)?)?;
        out.push(Section {
            vsize: read_u32(b, off + 8)?,
            va: read_u32(b, off + 12)?,
            raw_size: read_u32(b, off + 16)?,
            raw_ptr: read_u32(b, off + 20)?,
        });
    }
    Some(out)
}

/// RVA → 文件偏移。rva 落在某个节内时换算，否则 `None`。
fn rva_to_offset(sections: &[Section], rva: u32) -> Option<usize> {
    for s in sections {
        let span = s.vsize.max(s.raw_size);
        if rva >= s.va && rva < s.va.saturating_add(span) {
            return Some((u64::from(s.raw_ptr) + u64::from(rva - s.va)) as usize);
        }
    }
    None
}

/// 解析内存里的 PE 镜像，返回导入目录声明的 DLL 文件名（小写、去重、按出现顺序）。
///
/// 解析失败（不是 PE、结构越界、没有导入目录）返回 `None`，交由调用方决定
/// 「不打扰用户」——绝不能把一次读取失败误报成「运行库缺失」。
pub fn imported_dll_names(bytes: &[u8]) -> Option<Vec<String>> {
    // DOS 头：e_lfanew（PE 头偏移）在 0x3C。
    let pe_off = read_u32(bytes, 0x3c)? as usize;
    if bytes.get(pe_off..pe_off.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let coff = pe_off + 4;
    let num_sections = read_u16(bytes, coff + 2)? as usize;
    let opt_size = read_u16(bytes, coff + 16)? as usize;
    let opt = coff + 20;
    // 可选头魔数决定数据目录起点：PE32 / PE32+ 不同。
    let dd_base = match read_u16(bytes, opt)? {
        0x10b => opt + 96,  // PE32
        0x20b => opt + 112, // PE32+
        _ => return None,
    };
    // 数据目录第 0 项 = 导出表，第 1 项 = 导入表（RVA、Size 各 4 字节）。
    let import_rva = read_u32(bytes, dd_base + 8)?;
    let import_size = read_u32(bytes, dd_base + 12)?;
    if import_rva == 0 || import_size == 0 {
        return None;
    }
    let sections = parse_sections(bytes, opt.checked_add(opt_size)?, num_sections)?;

    let mut names: Vec<String> = Vec::new();
    let mut desc = rva_to_offset(&sections, import_rva)?;
    // IMAGE_IMPORT_DESCRIPTOR 共 20 字节；Name 字段在 +12。
    while let (Some(name_rva), Some(first_thunk), Some(orig_first_thunk)) = (
        read_u32(bytes, desc + 12),
        read_u32(bytes, desc),
        read_u32(bytes, desc + 16),
    ) {
        if name_rva == 0 && first_thunk == 0 && orig_first_thunk == 0 {
            break; // 全零描述符 = 列表结束
        }
        if name_rva != 0 {
            if let Some(off) = rva_to_offset(&sections, name_rva) {
                if let Some(name) = read_cstr(bytes, off) {
                    let n = name.to_ascii_lowercase();
                    if !n.is_empty() && !names.contains(&n) {
                        names.push(n);
                    }
                }
            }
        }
        desc += 20;
    }
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// 递归收集 `binary` 的导入闭包：把每个导入的 DLL 记入 `needed`；若该 DLL
/// 就在 `dir` 下（即本项目随附的组件），再深入解析它自己的导入表。
///
/// 深度上限 4 层：官方 whisper.cpp 构建最深也不过
/// `whisper-cli.exe → ggml.dll → ggml-cpu.dll` 三层。`visited` 去重，
/// 避免 DLL 互相导入时无限递归。
fn collect_import_closure(
    binary: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    needed: &mut Vec<String>,
    visited: &mut Vec<std::path::PathBuf>,
) {
    if depth > 4 || visited.iter().any(|v| v == binary) {
        return;
    }
    visited.push(binary.to_path_buf());
    let Ok(bytes) = std::fs::read(binary) else {
        return; // 读不到就当解析失败：绝不误报为「缺失」
    };
    let Some(imports) = imported_dll_names(&bytes) else {
        return;
    };
    for name in imports {
        if !needed.iter().any(|n| n == &name) {
            needed.push(name.clone());
        }
        // 只有「随附在同目录」的 DLL 才继续深挖；系统 DLL 不解析，
        // 省时也避免越权读系统目录。
        let local = dir.join(&name);
        if local.exists() {
            collect_import_closure(&local, dir, depth + 1, needed, visited);
        }
    }
}

/// 某个导入名能否被 Windows 加载器解析到。
///
/// 判定顺序：Windows 自带 API 集（`api-ms-win-*` / `ext-ms-*`，永远存在）→
/// 同目录 → System32。三者都不命中才认为缺失。
///
/// 这里**不**遍历完整 PATH：逐个目录 stat 既慢，又容易把「恰好同名的无关 DLL」
/// 当成可用；System32 + 同目录已覆盖 VC++ 运行库与随附 DLL 的全部正常落点。
pub fn dll_is_resolvable(name: &str, dir: &std::path::Path) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("api-ms-win-") || lower.starts_with("ext-ms-win-") {
        return true;
    }
    if dir.join(&lower).exists() {
        return true;
    }
    std::path::Path::new("C:/Windows/System32")
        .join(&lower)
        .exists()
}

/// 入口二进制**真正需要、却无法被加载器找到**的导入 DLL 列表（小写、去重）。
///
/// 这是启动体检 `check_whisper_cli_runtime_deps` 的判定核心：沿同目录 DLL
/// 递归展开导入闭包，再逐个判定可解析性。返回空表示「运行库齐备」。
/// 解析失败的二进制会被忽略（不误报），符合「只报告确定的缺失」原则。
pub fn missing_imports(entry: &std::path::Path) -> Vec<String> {
    let Some(dir) = entry.parent() else {
        return Vec::new();
    };
    let mut needed: Vec<String> = Vec::new();
    let mut visited: Vec<std::path::PathBuf> = Vec::new();
    collect_import_closure(entry, dir, 0, &mut needed, &mut visited);
    needed
        .into_iter()
        .filter(|dll| !dll_is_resolvable(dll, dir))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工拼一个最小 PE（1 个节、1 条导入描述符、导入 test.dll），
    /// 把 RVA→文件偏移的换算单独钉死。
    fn synthetic_pe() -> Vec<u8> {
        let mut b = vec![0u8; 0x600];
        b[0] = b'M';
        b[1] = b'Z';
        b[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes()); // e_lfanew
        b[0x40..0x44].copy_from_slice(b"PE\0\0");
        let coff = 0x44;
        b[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // NumberOfSections
        b[coff + 16..coff + 18].copy_from_slice(&0xF0u16.to_le_bytes()); // SizeOfOptionalHeader
        let opt = coff + 20; // 0x58
        b[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes()); // PE32+
        let dd = opt + 112;
        b[dd + 8..dd + 12].copy_from_slice(&0x1000u32.to_le_bytes()); // 导入表 RVA
        b[dd + 12..dd + 16].copy_from_slice(&40u32.to_le_bytes()); // 导入表 Size
        let sec = opt + 0xF0;
        b[sec..sec + 8].copy_from_slice(b".rdata\0\0");
        b[sec + 8..sec + 12].copy_from_slice(&0x200u32.to_le_bytes()); // VirtualSize
        b[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // VirtualAddress
        b[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes()); // SizeOfRawData
        b[sec + 20..sec + 24].copy_from_slice(&0x400u32.to_le_bytes()); // PointerToRawData
                                                                        // 导入描述符（RVA 0x1000 → 文件 0x400）
        let d0 = 0x400;
        b[d0..d0 + 4].copy_from_slice(&0x1030u32.to_le_bytes()); // OriginalFirstThunk
        b[d0 + 12..d0 + 16].copy_from_slice(&0x1020u32.to_le_bytes()); // Name RVA
        b[d0 + 16..d0 + 20].copy_from_slice(&0x1040u32.to_le_bytes()); // FirstThunk
                                                                       // 名字字符串（RVA 0x1020 → 文件 0x420）
        b[0x420..0x429].copy_from_slice(b"test.dll\0");
        b
    }

    /// 用给定的导入 DLL 名拼一个最小 PE（1 节、1 条导入描述符）。
    fn synthetic_pe_importing(dll: &str) -> Vec<u8> {
        let mut b = synthetic_pe();
        // 覆盖名字字符串区（原 test.dll 处），并清掉尾部残留
        let mut name = dll.as_bytes().to_vec();
        name.push(0);
        assert!(name.len() <= 0x1E0, "测试用 DLL 名过长");
        for i in 0..0x1E0 {
            b[0x420 + i] = 0;
        }
        b[0x420..0x420 + name.len()].copy_from_slice(&name);
        b
    }

    #[test]
    fn missing_imports_reports_unresolvable_dll() {
        let dir = std::env::temp_dir().join(format!("v2w_peimp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("entry.exe");
        std::fs::write(&entry, synthetic_pe_importing("v2w-nonexistent-xyz.dll")).unwrap();
        let missing = missing_imports(&entry);
        assert_eq!(missing, vec!["v2w-nonexistent-xyz.dll".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_imports_accepts_api_sets_and_local_dll() {
        let dir = std::env::temp_dir().join(format!("v2w_peimp2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 1) api-ms-win-* 永远视为可解析
        let e1 = dir.join("a.exe");
        std::fs::write(
            &e1,
            synthetic_pe_importing("api-ms-win-core-synch-l1-1-0.dll"),
        )
        .unwrap();
        assert!(missing_imports(&e1).is_empty());
        // 2) 同目录存在该 DLL → 可解析
        let e2 = dir.join("b.exe");
        std::fs::write(&e2, synthetic_pe_importing("local-only.dll")).unwrap();
        std::fs::write(dir.join("local-only.dll"), b"stub").unwrap();
        assert!(missing_imports(&e2).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_synthetic_pe_imports() {
        assert_eq!(
            imported_dll_names(&synthetic_pe()),
            Some(vec!["test.dll".to_string()])
        );
    }

    #[test]
    fn rejects_garbage_and_truncated() {
        assert_eq!(imported_dll_names(b""), None);
        assert_eq!(imported_dll_names(b"not a pe file at all"), None);
        // 签名正确但整体被截断 → 只能返回 None，绝不能 panic
        let mut pe = synthetic_pe();
        pe.truncate(0x80);
        assert_eq!(imported_dll_names(&pe), None);
    }

    /// 穷举截断点：合法的 PE 在任何位置被截断都只能返回 `None`，绝不能 panic。
    /// 解析端所有的索引读取都必须走有界检查——这条测试就是那道防线的回归网。
    #[test]
    fn truncation_at_every_offset_never_panics() {
        let pe = synthetic_pe();
        for cut in 0..pe.len() {
            let _ = imported_dll_names(&pe[..cut]);
        }
    }

    /// 字节翻转：随机破坏头部关键字段（e_lfanew / 节数 / 数据目录偏移等）
    /// 也不能 panic，只能返回 `None` 或一个（可能不完整的）列表。
    #[test]
    fn corrupted_header_fields_never_panic() {
        let base = synthetic_pe();
        // 这些位置分别覆盖 e_lfanew、COFF 节数/可选头大小、可选头魔数、数据目录项
        for off in [0x3c, 0x3d, 0x46, 0x54, 0x58, 0x60, 0xc8, 0xcc, 0xd0] {
            for val in [0x00u8, 0x01, 0x7f, 0x80, 0xff] {
                let mut pe = base.clone();
                if off < pe.len() {
                    pe[off] = val;
                    let _ = imported_dll_names(&pe);
                }
            }
        }
    }

    /// 真实系统文件冒烟：解析 `cmd.exe` 应能列出它导入的系统 DLL。
    /// 非 Windows / 文件缺失时跳过（CI 上不一定有）。
    ///
    /// 断言 `ntdll.dll` 而非 `kernel32.dll`：Win10/11 的可执行文件普遍走
    /// API Set 重定向（导入的是 `api-ms-win-*` 虚拟 DLL），但 `ntdll.dll`
    /// 始终是真实导入项，用它做锚点更稳。
    #[test]
    fn parses_real_windows_binary_if_present() {
        let p = std::path::Path::new("C:/Windows/System32/cmd.exe");
        let Ok(bytes) = std::fs::read(p) else {
            return;
        };
        let names = imported_dll_names(&bytes).expect("cmd.exe 应有导入表");
        assert!(
            names.iter().any(|n| n == "ntdll.dll"),
            "cmd.exe 应导入 ntdll.dll，实际: {names:?}"
        );
    }
}
