//! 外部 CLI 工具路径安全适配器
//!
//! 在 Windows 系统上，许多底层 C/C++ 预编译工具（如 whisper-cli, llama-cli, ffmpeg 等）
//! 内部依然使用 ANSI 窄字符标准库（fopen / std::ifstream / open），当系统处于
//! 中文代码页（CP936/GBK）且路径包含中文或特殊字符（如 `D:\程序\voice\...`）时，
//! 传入 UTF-8 路径会导致底层乱码并报错退出。
//!
//! 本模块提供高可靠性的路径转换：
//! 1. 优先使用 Windows 8.3 短路径名（GetShortPathNameW，纯 ASCII，0 拷贝，与原路径 100% 等价）；
//! 2. 若当前卷禁用 8.3 短路径，在纯 ASCII 安全目录中创建硬链接（同驱动器 0 磁盘占用）；
//! 3. 提供 safe_temp_dir()，避免当 Windows 用户名含中文时将临时文件建在无法写入的路径中。

use std::path::{Path, PathBuf};
use tracing::debug;

/// 获取适合传给外部 C/C++ CLI 的安全路径。
///
/// 若路径已经是纯 ASCII，原样返回（0 开销）。
/// 若在 Windows 上包含非 ASCII 字符，尝试转换为纯 ASCII 路径。
pub fn to_cli_safe_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return normalize_path_slashes(path);
    }

    let normalized = normalize_path_slashes(path);

    #[cfg(windows)]
    {
        let path_str = normalized.to_string_lossy();
        // 空格不需要处理：子进程收到的是 argv 数组，Windows 会保留参数内的空格。
        // 只有**非 ASCII** 才需要考虑别名（历史上这里把空格也判成不安全，导致
        // 带空格的路径白白走 8.3 / 硬链接 / 整份复制）。
        if path_str.is_ascii() {
            return normalized;
        }

        // 1. 尝试使用 Windows 8.3 短路径名 (GetShortPathNameW)
        if let Some(short) = get_win32_short_path(&normalized) {
            let short_str = short.to_string_lossy();
            if short_str.is_ascii() && short.exists() {
                debug!(original = ?normalized, short = ?short, "路径已安全转换为 Windows 8.3 短路径");
                return short;
            }
        }

        // 2. 兜底：**仅小文件**复制到纯 ASCII 临时目录。
        //
        // 这里刻意不再做两件事：
        // - 不再用 `DefineDosDeviceW` 把父目录映射成盘符。客户机上莫名多出的
        //   Z: 盘就是这么来的：映射从不删除，泄漏到重启；它还显示为目标目录所在
        //   卷的大小（与 E: 一样大），极具迷惑性。
        // - 不再在驱动器根目录建 `.v2w_alias` 硬链接：删除源文件后空间不回收，
        //   盘根还会多出隐藏目录。
        // 大文件（视频 / 模型）直接用原路径：ffmpeg、whisper 都是从 argv 取
        // UTF-8 路径，不需要 ASCII 别名；GB 级复制只会拖慢起播并吃满磁盘。
        const MAX_SAFE_COPY_BYTES: u64 = 64 * 1024 * 1024;
        if normalized.is_file() {
            let small = normalized
                .metadata()
                .map(|m| m.len() <= MAX_SAFE_COPY_BYTES)
                .unwrap_or(false);
            if small {
                if let Some(copied) = copy_to_safe_temp(&normalized) {
                    debug!(original = ?normalized, copied = ?copied, "小文件已缓存到纯 ASCII 临时目录");
                    return copied;
                }
            } else {
                debug!(original = ?normalized, "非 ASCII 大文件不做别名、不复制，直接使用原路径");
            }
        }
    }

    normalized
}

/// 规范化路径斜杠（Windows 上统一使用反斜杠 `\`，杜绝混合斜杠引起的 C++ 解析器 bug）
pub fn normalize_path_slashes(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if s.contains('/') {
            return PathBuf::from(s.replace('/', "\\"));
        }
    }
    path.to_path_buf()
}

/// 获取保证为纯 ASCII 的安全临时目录。
///
/// 若当前系统 temp_dir() 含有非 ASCII 字符（如用户名为中文），则智能回退到
/// ProgramData 或系统盘纯英文临时目录，防止 CLI 工具输出崩溃。
pub fn safe_temp_dir() -> PathBuf {
    let default_temp = std::env::temp_dir();

    #[cfg(windows)]
    {
        let temp_str = default_temp.to_string_lossy();
        if temp_str.is_ascii() && !temp_str.contains(' ') {
            return default_temp;
        }

        // 尝试转短路径
        if let Some(short) = get_win32_short_path(&default_temp) {
            let short_str = short.to_string_lossy();
            if short_str.is_ascii() {
                return short;
            }
        }

        // 回退 1: %LOCALAPPDATA%\Voice2Word\temp（用户私有、通常纯 ASCII）
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let p = PathBuf::from(local).join("Voice2Word").join("temp");
            if p.to_string_lossy().is_ascii() && std::fs::create_dir_all(&p).is_ok() {
                return p;
            }
        }

        // 回退 2: %ProgramData%\Voice2Word\temp（全机共享，权限宽松）
        if let Ok(prog_data) = std::env::var("ProgramData") {
            let p = PathBuf::from(prog_data).join("Voice2Word").join("temp");
            if std::fs::create_dir_all(&p).is_ok() {
                return p;
            }
        }

        // 回退 3: %PUBLIC%\Voice2Word\temp（C:\Users\Public 几乎恒为 ASCII）
        if let Ok(public) = std::env::var("PUBLIC") {
            let p = PathBuf::from(public).join("Voice2Word").join("temp");
            if p.to_string_lossy().is_ascii() && std::fs::create_dir_all(&p).is_ok() {
                return p;
            }
        }

        // 刻意**不再回退到 C:\Temp**：在系统盘根目录凭空建目录属于「对用户系统的
        // 意外改动」——客户的 Z 盘事故就是同一类问题的另一个版本。宁可沿用原
        // %TEMP%（哪怕含中文），也不往盘根写。
        debug!("未找到纯 ASCII 的临时目录回退，沿用系统 %TEMP%");
    }

    default_temp
}

#[cfg(windows)]
fn get_win32_short_path(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    extern "system" {
        fn GetShortPathNameW(
            lpszLongPath: *const u16,
            lpszShortPath: *mut u16,
            cchBuffer: u32,
        ) -> u32;
    }

    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if len > 0 && (len as usize) < buf.len() {
        use std::os::windows::ffi::OsStringExt;
        let os_str = std::ffi::OsString::from_wide(&buf[..len as usize]);
        Some(PathBuf::from(os_str))
    } else {
        None
    }
}

#[cfg(windows)]
fn copy_to_safe_temp(src: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut hasher);
    let hash = hasher.finish();

    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");

    let temp_dest = safe_temp_dir().join(format!("v2w_safe_{:x}.{}", hash, ext));
    if temp_dest.exists() {
        if let (Ok(m1), Ok(m2)) = (src.metadata(), temp_dest.metadata()) {
            if m1.len() == m2.len() {
                return Some(temp_dest);
            }
        }
        let _ = std::fs::remove_file(&temp_dest);
    }

    if std::fs::copy(src, &temp_dest).is_ok() {
        return Some(temp_dest);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path_slashes() {
        let p = Path::new("D:/models/whisper/test.bin");
        let norm = normalize_path_slashes(p);
        #[cfg(windows)]
        assert!(!norm.to_string_lossy().contains('/'));
    }

    #[test]
    fn test_safe_temp_dir() {
        let temp = safe_temp_dir();
        assert!(temp.exists());
    }

    /// 回归：**含空格的纯 ASCII 路径必须原样返回**。
    ///
    /// 旧实现把空格也判成「不安全」，于是带空格的模型/素材会白白走 8.3、
    /// 盘根硬链接，最后甚至整份复制到 %TEMP%。客户机上莫名多出的 Z: 盘
    /// （大小等于 E:）来自那条「把父目录映射成盘符」的策略——已整体删除。
    #[test]
    #[cfg(windows)]
    fn ascii_path_with_spaces_is_returned_as_is() {
        let dir = std::env::temp_dir().join("v2w space test");
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("a b.txt");
        std::fs::write(&file, b"x").ok();
        assert_eq!(
            to_cli_safe_path(&file),
            file,
            "含空格但纯 ASCII 的路径不应被转换/复制"
        );
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
