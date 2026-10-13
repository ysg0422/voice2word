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
            &format!("写入程序文件失败:\n{}\n\n错误: {}", target_exe.display(), e),
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
    let bat_content = format!(
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
        app = APP_NAME,
        exe = APP_EXE_NAME,
        dir = install_dir.display(),
    );
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
