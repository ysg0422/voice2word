//! Voice2Word Windows 原生向导式安装程序
//! 单文件独立安装包，内置最新主程序，自动创建桌面与开始菜单快捷方式，支持无管理员权限安装与干净卸载。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const APP_NAME: &str = "Voice2Word";
const APP_VERSION: &str = "0.1.3";
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
const MB_ICONINFORMATION: u32 = 0x00000040;
const MB_ICONQUESTION: u32 = 0x00000020;
const MB_ICONERROR: u32 = 0x00000010;
const IDOK: i32 = 1;
const IDYES: i32 = 6;

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
        PathBuf::from(local_app_data).join("Programs").join(APP_NAME)
    } else if let Ok(user_profile) = std::env::var("USERPROFILE") {
        PathBuf::from(user_profile).join("AppData").join("Local").join("Programs").join(APP_NAME)
    } else {
        PathBuf::from("C:\\Voice2Word")
    }
}

fn create_shortcut(target_exe: &Path, shortcut_path: &Path, working_dir: &Path) -> Result<(), String> {
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
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
        .status()
        .map_err(|e| format!("快捷方式创建失败: {}", e))?;

    if status.success() {
        Ok(())
    } else {
        Err("创建快捷方式返回非零退出码".to_string())
    }
}

fn run_installer() {
    let install_dir = get_install_dir();
    let welcome_msg = format!(
        "欢迎使用 {app} v{ver} 安装向导！\n\n\
        本程序将把 {app} 安装至您的电脑：\n\
        {dir}\n\n\
        安装完成后将自动创建：\n\
        • 桌面快捷方式\n\
        • 开始菜单快捷方式\n\
        • 控制面板卸载项\n\n\
        点击【确定】立即开始安装，点击【取消】退出向导。",
        app = APP_NAME,
        ver = APP_VERSION,
        dir = install_dir.display()
    );

    let res = message_box(
        &format!("{app} v{ver} 安装程序", app = APP_NAME, ver = APP_VERSION),
        &welcome_msg,
        MB_OKCANCEL | MB_ICONQUESTION,
    );

    if res != IDOK {
        return;
    }

    // 1. 创建目标目录
    if let Err(e) = fs::create_dir_all(&install_dir) {
        message_box(
            "安装失败",
            &format!("无法创建安装目录:\n{}\n\n错误: {}", install_dir.display(), e),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    // 2. 释放主执行程序
    let target_exe = install_dir.join(APP_EXE_NAME);
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
        let _ = fs::copy(&src_config, &dst_config);
    }

    let src_tools = current_dir.join("tools");
    let dst_tools = install_dir.join("tools");
    if src_tools.exists() {
        let _ = fs::create_dir_all(&dst_tools);
        if let Ok(entries) = fs::read_dir(&src_tools) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    let _ = fs::copy(&p, dst_tools.join(entry.file_name()));
                }
            }
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
        rmdir /s /q \"{dir}\" >nul 2>&1\r\n\
        msg * \"{app} 已成功从您的电脑中卸载。\"\r\n",
        app = APP_NAME,
        exe = APP_EXE_NAME,
        dir = install_dir.display(),
    );
    let _ = fs::write(&uninstall_bat, bat_content);

    // 5. 创建桌面快捷方式
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        let desktop = PathBuf::from(user_profile).join("Desktop");
        if desktop.exists() {
            let desktop_shortcut = desktop.join(format!("{}.lnk", APP_NAME));
            let _ = create_shortcut(&target_exe, &desktop_shortcut, &install_dir);
        }
    }

    // 6. 创建开始菜单快捷方式
    if let Ok(app_data) = std::env::var("APPDATA") {
        let start_menu = PathBuf::from(app_data)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs");
        if start_menu.exists() {
            let start_shortcut = start_menu.join(format!("{}.lnk", APP_NAME));
            let _ = create_shortcut(&target_exe, &start_shortcut, &install_dir);
        }
    }

    // 7. 注册 Windows 控制面板卸载项 (当前用户)
    let reg_cmd = format!(
        "reg add \"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app}\" /v \"DisplayName\" /d \"{app} (音视频智能字幕生成器)\" /f >nul && \
         reg add \"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app}\" /v \"DisplayVersion\" /d \"{ver}\" /f >nul && \
         reg add \"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app}\" /v \"Publisher\" /d \"Voice2Word Team\" /f >nul && \
         reg add \"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app}\" /v \"InstallLocation\" /d \"{dir}\" /f >nul && \
         reg add \"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{app}\" /v \"UninstallString\" /d \"\\\"{uninst}\\\"\" /f >nul",
        app = APP_NAME,
        ver = APP_VERSION,
        dir = install_dir.display(),
        uninst = uninstall_bat.display(),
    );
    let _ = Command::new("cmd").args(["/c", &reg_cmd]).status();

    // 8. 询问是否立即启动
    let finish_msg = format!(
        "{app} v{ver} 已成功安装到您的电脑！\n\n\
        • 安装位置: {dir}\n\
        • 桌面快捷方式已创建\n\
        • 开始菜单项已创建\n\n\
        是否立即启动 {app}？",
        app = APP_NAME,
        ver = APP_VERSION,
        dir = install_dir.display()
    );

    let run_now = message_box(
        &format!("{app} 安装成功", app = APP_NAME),
        &finish_msg,
        MB_YESNO | MB_ICONINFORMATION,
    );

    if run_now == IDYES {
        let _ = Command::new(&target_exe).current_dir(&install_dir).spawn();
    }
}

fn main() {
    run_installer();
}
