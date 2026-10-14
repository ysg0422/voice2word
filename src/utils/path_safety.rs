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
        if path_str.is_ascii() && !path_str.contains(' ') {
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

        // 2. 如果短路径依然包含非 ASCII，尝试在安全临时目录创建硬链接 (仅适用于普通文件)
        if normalized.is_file() {
            if let Some(alias) = create_safe_file_alias(&normalized) {
                debug!(original = ?normalized, alias = ?alias, "路径已创建纯 ASCII 安全文件硬链接别名");
                return alias;
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

        // 回退 1: ProgramData（Windows 默认全权限且恒为纯英文）
        if let Ok(prog_data) = std::env::var("ProgramData") {
            let p = PathBuf::from(prog_data).join("Voice2Word").join("temp");
            if std::fs::create_dir_all(&p).is_ok() {
                return p;
            }
        }

        // 回退 2: 系统盘 C:\Temp
        let c_temp = PathBuf::from("C:\\Temp");
        if std::fs::create_dir_all(&c_temp).is_ok() {
            return c_temp;
        }
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
fn create_safe_file_alias(src: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut hasher);
    let hash = hasher.finish();

    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");

    // 优先在源文件相同驱动器的根临时目录创建硬链接（硬链接不可跨卷）
    // 注意：PrefixComponent.as_os_str() 返回的是 "D:"，必须显式补上 "\" 成为 "D:\" 根目录！
    // 否则拼接出的 "D:.v2w_alias" 是驱动器相对路径，会被当成不存在的非法路径。
    let mut components = src.components();
    if let Some(std::path::Component::Prefix(prefix)) = components.next() {
        let mut drive_root = PathBuf::from(prefix.as_os_str());
        drive_root.push("\\");
        let alias_dir = drive_root.join(".v2w_alias");
        if std::fs::create_dir_all(&alias_dir).is_ok() {
            let alias = alias_dir.join(format!("m_{:x}.{}", hash, ext));
            if alias.exists() {
                return Some(alias);
            }
            if std::fs::hard_link(src, &alias).is_ok() {
                return Some(alias);
            }
        }
    }

    // 回退策略：对于小于等于 128MB 的文件（如音频 wav 输入文件），直接安全复制到纯英文 safe_temp_dir
    if let Ok(meta) = src.metadata() {
        if meta.len() <= 128 * 1024 * 1024 {
            let temp_dest = safe_temp_dir().join(format!("v2w_safe_{:x}.{}", hash, ext));
            if temp_dest.exists() {
                return Some(temp_dest);
            }
            if std::fs::copy(src, &temp_dest).is_ok() {
                return Some(temp_dest);
            }
        }
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

    #[test]
    #[cfg(windows)]
    fn test_drive_root_slash() {
        let p = Path::new("D:\\测试目录\\model.bin");
        let mut components = p.components();
        if let Some(std::path::Component::Prefix(prefix)) = components.next() {
            let mut drive_root = PathBuf::from(prefix.as_os_str());
            drive_root.push("\\");
            assert_eq!(drive_root.to_str().unwrap(), "D:\\");
            let alias_dir = drive_root.join(".v2w_alias");
            assert_eq!(alias_dir.to_str().unwrap(), "D:\\.v2w_alias");
        }
    }
}
