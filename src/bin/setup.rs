//! Voice2Word Windows 原生向导式安装程序
//! 单文件独立安装包，内置最新主程序，自动创建桌面与开始菜单快捷方式，支持无管理员权限安装与干净卸载。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const APP_NAME: &str = "Voice2Word";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_EXE_NAME: &str = "voice2word.exe";

// 内置由 cargo build --release 生成的主程序二进制
static MAIN_EXE_BYTES: &[u8] = include_bytes!("../../target/release/voice2word.exe");

#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(
        hwnd: *mut std::ffi::c_void,
        lp_text: *const u16,
        lp_caption: *const u16,
        u_type: u32,
    ) -> i32;
}

const MB_OK: u32 = 0x00000000;
const MB_OKCANCEL: u32 = 0x00000001;
const MB_YESNO: u32 = 0x00000004;
const MB_YESNOCANCEL: u32 = 0x00000003;
const MB_ICONINFORMATION: u32 = 0x00000040;
const MB_ICONQUESTION: u32 = 0x00000020;
const MB_ICONERROR: u32 = 0x00000010;
const IDOK: i32 = 1;
const IDYES: i32 = 6;
const IDNO: i32 = 7;

fn to_wide_chars(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn message_box(title: &str, text: &str, flags: u32) -> i32 {
    let wide_title = to_wide_chars(title);
    let wide_text = to_wide_chars(text);
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide_text.as_ptr(),
            wide_title.as_ptr(),
            flags,
        )
    }
}

fn get_install_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(local_app_data)
            .join("Programs")
            .join(APP_NAME)
    } else if let Ok(user_profile) = std::env::var("USERPROFILE") {
        PathBuf::from(user_profile)
            .join("AppData")
            .join("Local")
            .join("Programs")
            .join(APP_NAME)
    } else {
        PathBuf::from("C:\\Voice2Word")
    }
}

fn select_install_dir(default_dir: &Path) -> Option<PathBuf> {
    // 用户选择的是"安装位置"（父目录），最终会在其中创建 Voice2Word 子目录，
    // 以保证卸载时只删除专属目录，不波及其他文件。
    let default_parent = default_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_dir.to_path_buf());
    let title = format!("{APP_NAME} v{APP_VERSION} 安装程序");
    let choice = message_box(
        &title,
        &format!(
            "欢迎使用 {app} v{ver} 安装向导！\n\n默认安装位置：\n{dir}\n\n点击【是】使用默认位置安装；\n点击【否】自定义安装位置（可选择任意磁盘/目录）；\n点击【取消】退出向导。",
            app = APP_NAME,
            ver = APP_VERSION,
            dir = default_dir.display()
        ),
        MB_YESNOCANCEL | MB_ICONQUESTION,
    );

    let parent = match choice {
        IDYES => default_parent,
        IDNO => pick_directory(&default_parent)?,
        _ => return None,
    };
    let install_dir = resolve_install_dir(&parent);

    let confirm = message_box(
        &title,
        &format!(
            "即将把 {app} v{ver} 安装到：\n{dir}\n\n安装完成后将创建桌面快捷方式、开始菜单快捷方式与卸载项。\n注意：卸载时会删除上述安装目录（含其中的用户数据）。\n\n点击【确定】开始安装，点击【取消】退出向导。",
            app = APP_NAME,
            ver = APP_VERSION,
            dir = install_dir.display()
        ),
        MB_OKCANCEL | MB_ICONQUESTION,
    );

    if confirm == IDOK {
        // 目标目录已存在、非空、且不像本程序的旧安装时，再确认一次。
        //
        // 卸载脚本会 `rmdir /s /q` 整个安装目录。若用户把一个「已经装了别的
        // 东西」的目录选成安装位置，卸载就会连带删掉那些文件——而安装当时只
        // 提示过一次「含其中的用户数据」，用户很容易忽略。这里用客观判据
        // （目录里有没有本程序 exe）区分「旧安装」与「别人的目录」。
        if is_existing_foreign_dir(&install_dir) {
            let proceed = message_box(
                &title,
                &format!(
                    "所选目录已存在，且里面没有 {app}（{exe}）：\n{dir}\n\n\
                     它可能是您自己的目录。{app} 只写入自己的文件，但「卸载时会删除\n\
                     整个目录」，其中原有文件也会一并消失，且不可恢复。\n\n\
                     确定仍要安装到这个目录吗？",
                    app = APP_NAME,
                    exe = APP_EXE_NAME,
                    dir = install_dir.display()
                ),
                MB_YESNO | MB_ICONQUESTION,
            );
            if proceed != IDYES {
                return None;
            }
        }
        Some(install_dir)
    } else {
        None
    }
}

fn resolve_install_dir(parent: &Path) -> PathBuf {
    // 若所选目录本身就叫 Voice2Word，则直接使用，避免出现 ...\Voice2Word\Voice2Word。
    match parent.file_name() {
        Some(name) if name.to_string_lossy().eq_ignore_ascii_case(APP_NAME) => parent.to_path_buf(),
        _ => parent.join(APP_NAME),
    }
}

/// 目标目录是否「已存在、非空、且不像本程序的历史安装目录」。
///
/// 只认客观事实：目录里有没有 `voice2word.exe`。卸载脚本会 `rmdir /s /q` 整个安装
/// 目录，因此把一个「已经装了别的东西」的目录当安装位置，卸载就会连带删掉那些文件。
/// 抽成纯函数是为了能对着临时目录单测，不必真去弹窗。
fn is_existing_foreign_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    // 里面已有本程序 exe → 大概率是自己的旧安装（会被覆盖），不算陌生目录。
    if dir.join(APP_EXE_NAME).is_file() {
        return false;
    }
    match fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        // 读不到目录内容（权限等）就不臆断，按「不陌生」处理，不额外拦用户。
        Err(_) => false,
    }
}

fn pick_directory(initial: &Path) -> Option<PathBuf> {
    // 通过 PowerShell 的 FolderBrowserDialog 让用户可视化选择安装目录。
    let initial = initial.to_string_lossy().replace('\'', "''");
    let script = format!(
        "try {{ [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 }} catch {{}}; Add-Type -AssemblyName System.Windows.Forms | Out-Null; $dlg = New-Object System.Windows.Forms.FolderBrowserDialog; $dlg.Description = '选择 {APP_NAME} 的安装位置（将在其中创建 {APP_NAME} 文件夹）'; $dlg.SelectedPath = '{initial}'; $dlg.ShowNewFolderButton = $true; if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{ [Console]::Out.Write($dlg.SelectedPath) }}",
    );

    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-STA",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .output()
        .ok()?;

    let chosen = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if chosen.is_empty() {
        None
    } else {
        Some(PathBuf::from(chosen))
    }
}

fn create_shortcut(
    target_exe: &Path,
    shortcut_path: &Path,
    working_dir: &Path,
) -> Result<(), String> {
    let target = target_exe.to_string_lossy().replace('\'', "''");
    let shortcut = shortcut_path.to_string_lossy().replace('\'', "''");
    let work = working_dir.to_string_lossy().replace('\'', "''");

    let script = format!(
        "$WshShell = New-Object -ComObject WScript.Shell; \
         $Shortcut = $WshShell.CreateShortcut('{shortcut}'); \
         $Shortcut.TargetPath = '{target}'; \
         $Shortcut.WorkingDirectory = '{work}'; \
         $Shortcut.Description = '{APP_NAME} - 音视频智能字幕生成器'; \
         $Shortcut.Save()",
        shortcut = shortcut,
        target = target,
        work = work,
    );

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .status()
        .map_err(|e| format!("快捷方式创建失败: {}", e))?;

    if status.success() {
        Ok(())
    } else {
        Err("创建快捷方式返回非零退出码".to_string())
    }
}

/// 批处理里 `%` 的**唯一**转义方式：写两遍（`%%`）。
///
/// cmd 即使把路径放在双引号里也照样展开 `%VAR%` 与批处理参数 `%1`／`%*`——
/// 双引号挡不住 `%`。安装目录是用户自选的，完全可能含 `%`（例如
/// `D:\100%\Voice2Word`）；不转义的话，卸载脚本去 `echo`／`msg`／`rmdir`
/// 的是展开后的**另一个路径**。
fn escape_batch_percent(s: &str) -> String {
    s.replace('%', "%%")
}

/// 生成卸载脚本内容。抽成纯函数是为了让「路径转义」这一处能被单测钉死。
fn build_uninstall_script(app: &str, exe: &str, install_dir: &Path) -> String {
    let dir = escape_batch_percent(&install_dir.display().to_string());
    format!(
        "@echo off\r\n\
        chcp 65001 >nul\r\n\
        echo 正在卸载 {app}...\r\n\
        taskkill /f /im {exe} >nul 2>&1\r\n\
        timeout /t 1 /nobreak >nul\r\n\
        reg delete \"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app}\" /f >nul 2>&1\r\n\
        del /f /q \"%USERPROFILE%\\Desktop\\{app}.lnk\" >nul 2>&1\r\n\
        del /f /q \"%APPDATA%\\Microsoft\\Windows\\Start Menu\\Programs\\{app}.lnk\" >nul 2>&1\r\n\
        echo 即将删除 {app} 的安装目录（其中包含您的用户数据）：\"{dir}\"\r\n\
        msg * \"{app} 卸载即将完成：将删除以下目录（其中包含您的用户数据），删除后不可恢复：{dir}\"\r\n\
        timeout /t 5 /nobreak >nul\r\n\
        rmdir /s /q \"{dir}\" >nul 2>&1\r\n\
        echo {app} 已从您的电脑中卸载。\r\n",
        app = app,
        exe = exe,
        dir = dir,
    )
}

/// 「写入目标 exe 失败」时的可操作解释。
///
/// 覆盖安装最常见的失败不是磁盘满，而是**目标程序正在运行**：Windows 下已被加载
/// 的 exe 会被加锁，`fs::write` 直接返回共享冲突。原来的提示只有系统原文
/// （「另一个程序正在使用此文件」），用户不知道该关哪个东西，只能干瞪眼。
/// 这里按 Win32 错误码把话讲清楚。其它错误码保持原文，不臆测原因。
fn write_failure_hint(err: &std::io::Error) -> String {
    match err.raw_os_error() {
        // ERROR_SHARING_VIOLATION(32) / ERROR_LOCK_VIOLATION(33)
        Some(32) | Some(33) => format!(
            "{APP_NAME} 正在运行，安装程序无法覆盖它的 exe。\n\
             请先退出正在运行的 {APP_NAME}（含任务栏/托盘里没关掉的窗口），再重新运行安装程序。"
        ),
        // ERROR_ACCESS_DENIED(5)
        Some(5) => "没有写入权限。请改选有写权限的目录（默认的「用户目录」位置无需管理员权限），\
             或以管理员身份运行安装程序。"
            .to_string(),
        _ => "写入程序文件失败".to_string(),
    }
}

fn run_installer() {
    let default_dir = get_install_dir();
    // 步骤 1：选择安装位置（默认目录，或由用户自定义）。
    let install_dir = match select_install_dir(&default_dir) {
        Some(dir) => dir,
        None => return,
    };
    // 记录非致命步骤的失败原因，最终在完成提示里如实反馈，避免假装成功。
    let mut warnings: Vec<String> = Vec::new();

    // 1. 创建目标目录
    if let Err(e) = fs::create_dir_all(&install_dir) {
        message_box(
            "安装失败",
            &format!(
                "无法创建安装目录:\n{}\n\n错误: {}",
                install_dir.display(),
                e
            ),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    // 2. 释放主执行程序
    let target_exe = install_dir.join(APP_EXE_NAME);
    if MAIN_EXE_BYTES.len() < 1024 * 1024 {
        message_box(
            "安装失败",
            "安装程序数据不完整，请重新下载官方正式发布的安装包！",
            MB_OK | MB_ICONERROR,
        );
        return;
    }
    if let Err(e) = fs::write(&target_exe, MAIN_EXE_BYTES) {
        message_box(
            "安装失败",
            &format!(
                "{}\n\n文件: {}\n错误: {}",
                write_failure_hint(&e),
                target_exe.display(),
                e
            ),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    // 3. 复制 tools 目录与 config.toml（如存在）
    let current_dir = std::env::current_dir().unwrap_or_default();
    let src_config = current_dir.join("config.toml");
    let dst_config = install_dir.join("config.toml");
    if src_config.exists() && !dst_config.exists() {
        if let Err(e) = fs::copy(&src_config, &dst_config) {
            warnings.push(format!("配置文件 config.toml 复制失败：{e}"));
        }
    }

    let src_tools = current_dir.join("tools");
    let dst_tools = install_dir.join("tools");
    if src_tools.exists() {
        if let Err(e) = fs::create_dir_all(&dst_tools) {
            warnings.push(format!("创建 tools 目录失败：{e}"));
        }
        match fs::read_dir(&src_tools) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() {
                        if let Err(e) = fs::copy(&p, dst_tools.join(entry.file_name())) {
                            warnings.push(format!(
                                "复制 {} 失败：{e}",
                                entry.file_name().to_string_lossy()
                            ));
                        }
                    }
                }
            }
            Err(e) => warnings.push(format!("读取 tools 目录失败：{e}")),
        }
    }

    // 4. 生成卸载脚本
    let uninstall_bat = install_dir.join("uninstall.bat");
    let bat_content = build_uninstall_script(APP_NAME, APP_EXE_NAME, &install_dir);
    if let Err(e) = fs::write(&uninstall_bat, bat_content) {
        warnings.push(format!("卸载脚本 uninstall.bat 写入失败：{e}"));
    }

    // 5. 创建桌面快捷方式
    let mut desktop_ok = false;
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        let desktop = PathBuf::from(user_profile).join("Desktop");
        if desktop.exists() {
            let desktop_shortcut = desktop.join(format!("{}.lnk", APP_NAME));
            match create_shortcut(&target_exe, &desktop_shortcut, &install_dir) {
                Ok(()) => desktop_ok = true,
                Err(e) => warnings.push(format!("桌面快捷方式创建失败：{e}")),
            }
        } else {
            warnings.push("未找到桌面目录，已跳过桌面快捷方式".to_string());
        }
    } else {
        warnings.push("未读取到 USERPROFILE，已跳过桌面快捷方式".to_string());
    }

    // 6. 创建开始菜单快捷方式
    let mut start_menu_ok = false;
    if let Ok(app_data) = std::env::var("APPDATA") {
        let start_menu = PathBuf::from(app_data)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs");
        if start_menu.exists() {
            let start_shortcut = start_menu.join(format!("{}.lnk", APP_NAME));
            match create_shortcut(&target_exe, &start_shortcut, &install_dir) {
                Ok(()) => start_menu_ok = true,
                Err(e) => warnings.push(format!("开始菜单快捷方式创建失败：{e}")),
            }
        } else {
            warnings.push("未找到开始菜单目录，已跳过开始菜单快捷方式".to_string());
        }
    } else {
        warnings.push("未读取到 APPDATA，已跳过开始菜单快捷方式".to_string());
    }

    // 7. 注册 Windows 控制面板卸载项 (当前用户)
    //
    // # 为什么不再拼一条 `cmd /c reg add ... && ...`
    //
    // 安装目录是**用户自己选的**。以前把路径内插进一条 cmd 字符串、用双引号包裹，
    // 而 cmd 的双引号**挡不住 `%` 展开**：装到形如 `D:\100%\app` 的目录时，注册表里的
    // 卸载路径会变成展开后的别的字符串；路径里若含 `"`，还会直接把命令结构拆坏。
    // 现在改成逐项调用 `reg.exe`（参数数组、不经 shell）：键与值都是独立 argv，
    // 没有解析层需要绕过——与「媒体管线一律逐参传」同一条经验。
    let uninstall_key =
        format!("HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{APP_NAME}");
    let registry_entries: [(&str, String); 5] = [
        ("DisplayName", format!("{APP_NAME} (音视频智能字幕生成器)")),
        ("DisplayVersion", APP_VERSION.to_string()),
        ("Publisher", "Voice2Word Team".to_string()),
        ("InstallLocation", install_dir.display().to_string()),
        // 卸载串必须自带引号：控制面板会把它当命令行执行，路径含空格时不加引号会断。
        (
            "UninstallString",
            format!("\"{}\"", uninstall_bat.display()),
        ),
    ];
    for (name, value) in registry_entries {
        match Command::new("reg")
            .args(["add", &uninstall_key, "/v", name, "/d", &value, "/f"])
            .output()
        {
            Ok(out) if out.status.success() => {}
            Ok(out) => warnings.push(format!(
                "注册控制面板卸载项 {name} 失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => {
                // 连 `reg.exe` 都起不来（PATH 里没有、被策略拦），后面几条必然同样失败，
                // 报一条就够，不要把同一个原因重复五遍。
                warnings.push(format!("注册控制面板卸载项失败：{e}"));
                break;
            }
        }
    }

    // 8. 询问是否立即启动
    let mut finish_lines = vec![
        format!("{APP_NAME} v{APP_VERSION} 已成功安装到您的电脑！"),
        String::new(),
        format!("• 安装位置: {}", install_dir.display()),
    ];
    if desktop_ok {
        finish_lines.push("• 桌面快捷方式已创建".to_string());
    }
    if start_menu_ok {
        finish_lines.push("• 开始菜单项已创建".to_string());
    }
    if !warnings.is_empty() {
        finish_lines.push(String::new());
        finish_lines.push("以下步骤未完全成功，请留意：".to_string());
        for w in warnings.iter().take(5) {
            finish_lines.push(format!("· {w}"));
        }
        if warnings.len() > 5 {
            finish_lines.push(format!("· 另有 {} 项提示已省略", warnings.len() - 5));
        }
    }
    finish_lines.push(String::new());
    finish_lines.push(format!("是否立即启动 {APP_NAME}？"));
    let finish_msg = finish_lines.join("\n");

    let run_now = message_box(
        &format!("{app} 安装成功", app = APP_NAME),
        &finish_msg,
        MB_YESNO | MB_ICONINFORMATION,
    );

    if run_now == IDYES {
        if let Err(e) = Command::new(&target_exe).current_dir(&install_dir).spawn() {
            message_box(
                "启动失败",
                &format!("启动 {APP_NAME} 失败：\n{e}\n\n您可以稍后从桌面快捷方式手动启动。"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

fn main() {
    run_installer();
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn resolve_appends_app_folder_for_generic_parent() {
        assert_eq!(
            resolve_install_dir(Path::new("D:\\Apps")),
            PathBuf::from("D:\\Apps").join(APP_NAME)
        );
    }

    #[test]
    fn resolve_keeps_existing_app_folder() {
        assert_eq!(
            resolve_install_dir(Path::new("D:\\Custom\\Voice2Word")),
            PathBuf::from("D:\\Custom\\Voice2Word")
        );
    }

    #[test]
    fn resolve_is_case_insensitive_on_app_folder_name() {
        assert_eq!(
            resolve_install_dir(Path::new("D:\\voice2word")),
            PathBuf::from("D:\\voice2word")
        );
    }

    #[test]
    fn resolve_on_drive_root_appends_app_folder() {
        assert_eq!(
            resolve_install_dir(Path::new("D:\\")),
            PathBuf::from("D:\\").join(APP_NAME)
        );
    }

    #[test]
    fn default_install_dir_is_dedicated_app_folder() {
        let dir = get_install_dir();
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some(APP_NAME));
    }
}

/// 安装器健壮性回归：路径转义与「写入被占用」的提示。
///
/// 刻意与上面的向导测试分开、且不 `use super::*`：只显式引入被测函数，
/// 这样新增的依赖不会悄悄把整份模块塞进来。
#[cfg(test)]
mod robustness_tests {
    use super::{
        build_uninstall_script, escape_batch_percent, is_existing_foreign_dir, write_failure_hint,
    };
    use super::{APP_EXE_NAME, APP_NAME};
    use std::path::Path;

    #[test]
    fn foreign_dir_detected_when_nonempty_without_our_exe() {
        let dir = std::env::temp_dir().join(format!("v2w_setup_foreign_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 空目录：正常安装路径，不算陌生
        assert!(!is_existing_foreign_dir(&dir));
        // 放了用户自己的文件：陌生，卸载会连它一起删 → 必须拦
        std::fs::write(dir.join("my_notes.txt"), b"keep me").unwrap();
        assert!(is_existing_foreign_dir(&dir));
        // 放进本程序 exe：视作旧安装（覆盖安装场景），不再拦
        std::fs::write(dir.join(APP_EXE_NAME), b"exe").unwrap();
        assert!(!is_existing_foreign_dir(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nonexistent_dir_is_not_foreign() {
        let dir = std::env::temp_dir().join(format!("v2w_setup_absent_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!is_existing_foreign_dir(&dir));
    }

    #[test]
    fn escape_batch_percent_doubles_every_percent() {
        assert_eq!(escape_batch_percent("a%b%c"), "a%%b%%c");
        assert_eq!(escape_batch_percent("no percent"), "no percent");
        // 参数与变量引用同样要被转义，否则脚本会展开成别的东西
        assert_eq!(escape_batch_percent("%1 %* %PATH%"), "%%1 %%* %%PATH%%");
    }

    #[test]
    fn uninstall_script_escapes_percent_in_install_dir() {
        let script = build_uninstall_script(
            APP_NAME,
            APP_EXE_NAME,
            Path::new(r"D:\100% done\Voice2Word"),
        );
        // cmd 运行时会把 %% 还原成一个 %，所以脚本里必须写两个
        assert!(
            script.contains(r#""D:\100%% done\Voice2Word""#),
            "路径里的 % 必须转义成 %%：{script}"
        );
        // 绝不能残留未转义的单个 % —— 那会让 cmd 去操作另一个路径
        assert!(
            !script.contains("100% done"),
            "存在未转义的 %，卸载会指向错误目录：{script}"
        );
    }

    #[test]
    fn uninstall_script_keeps_plain_path_intact() {
        let script =
            build_uninstall_script(APP_NAME, APP_EXE_NAME, Path::new(r"C:\Apps\Voice2Word"));
        assert!(
            script.contains(r#"rmdir /s /q "C:\Apps\Voice2Word""#),
            "无 % 的路径不得被改动或被引号破坏：{script}"
        );
    }

    #[test]
    fn write_failure_hint_explains_locked_target() {
        // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION
        for code in [32, 33] {
            let err = std::io::Error::from_raw_os_error(code);
            let hint = write_failure_hint(&err);
            assert!(
                hint.contains("正在运行"),
                "占用错误应提示先关闭程序：{hint}"
            );
            assert!(hint.contains(APP_NAME));
        }
    }

    #[test]
    fn write_failure_hint_explains_access_denied() {
        // ERROR_ACCESS_DENIED
        let hint = write_failure_hint(&std::io::Error::from_raw_os_error(5));
        assert!(hint.contains("权限"), "拒绝访问应提示权限/换目录：{hint}");
    }

    #[test]
    fn write_failure_hint_keeps_unknown_cause_plain() {
        // 其它错误码不臆测原因，保持中性措辞
        let hint = write_failure_hint(&std::io::Error::from_raw_os_error(1));
        assert_eq!(hint, "写入程序文件失败");
    }
}
