//! Voice2Word Windows 原生安装程序（单窗口）。
//! 一屏装完：打开就看到安装位置，可点【选择其他位置】改路径，点【安装】后就地显示进度，
//! 装完同一个窗口里直接启动，全程不弹向导式对话框（也没有控制台黑窗）。
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
        IDNO => match pick_directory(&default_parent, std::ptr::null_mut()) {
            PickOutcome::Picked(picked) => picked,
            // 取消：退出向导（和以前一样）。
            PickOutcome::Canceled => return None,
            // 弹不出选择框：只提示一次，然后保持「当前路径」= 默认位置继续走流程，
            // 绝不退回「起 PowerShell 同步等」那条会卡死界面的路。
            PickOutcome::Failed => {
                message_box(
                    &title,
                    "无法打开系统文件夹选择框，将使用默认安装位置。",
                    MB_OK | MB_ICONERROR,
                );
                default_parent
            }
        },
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

/// 选目录的结果。
///
/// 分三态是为了不把「用户按了取消」也报成失败：取消是正常操作（保持当前路径、
/// 不打扰用户），只有真弹不出框才需要提示一次。
enum PickOutcome {
    Picked(PathBuf),
    Canceled,
    Failed,
}

/// 选择框的初始目录必须是**已存在**的目录。
///
/// shell 的树里没有不存在的路径，`BFFM_SETSELECTIONW` 会被静默丢掉、对话框退回默认
/// 位置；而默认安装目录（`...\Programs\Voice2Word`）在装之前正好就是不存在的那种。
/// 所以向上找到最接近的已存在祖先；一个都不存在就返回原值，交给对话框选默认位置。
#[cfg(windows)]
fn nearest_existing_dir(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.is_dir() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => return path.to_path_buf(),
        }
    }
}

/// 初始目录 → 选择框要用的 NUL 结尾宽字符串。
///
/// 抽出来是因为这块内存必须活到 `SHBrowseForFolderW` 返回（回调按指针读它），
/// 而「Path 怎么变成宽字符」用不着真弹窗就能验证。
#[cfg(windows)]
fn initial_selection_wide(path: &Path) -> Vec<u16> {
    if path.as_os_str().is_empty() {
        // 没有初始目录就不传：让选中项留在默认位置，而不是塞一个空串进去解析。
        return Vec::new();
    }
    to_wide_chars(&path.to_string_lossy())
}

/// 选择框回填的宽字符缓冲 → 路径。
///
/// `SHGetPathFromIDListW` 只保证写一个 NUL 结尾的串，缓冲尾部是清零时留下的 0，
/// 不能算路径内容；按第一个 NUL 截断，截断后为空说明没拿到路径。
#[cfg(windows)]
fn wide_to_path(buffer: &[u16]) -> Option<PathBuf> {
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buffer[..len])))
}

/// 原生「浏览文件夹」选择框：直接调 shell32 的 `SHBrowseForFolderW`。
///
/// # 为什么不用原来的 PowerShell 版
///
/// 那条路要 `Command::new("powershell").output()`：先起进程、再加载 .NET 程序集，
/// 然后**在窗口线程上同步等它退出**。等的这几秒里窗口一条消息都不处理，Windows 就
/// 把标题画成「（未响应）」；而且那个 Shell 对话框也没法指定 owner，于是选择框反而
/// 被卡住的主窗口压住。
///
/// `SHBrowseForFolderW` 是 shell32 自己的模态对话框：它在本线程跑自己的消息泵，
/// 主窗口的消息照常被处理，也不会多出第二个进程 —— 全程不阻塞。
/// `BROWSEINFOW.hwndOwner` 传安装窗口的 HWND，它就模态于该窗口、永远在上面。
#[cfg(windows)]
mod folder_picker {
    use super::{
        initial_selection_wide, nearest_existing_dir, to_wide_chars, wide_to_path, PickOutcome,
        APP_NAME,
    };
    use std::ffi::c_void;
    use std::path::Path;

    type Hwnd = *mut c_void;
    /// `LPITEMIDLIST`／`PIDLIST_ABSOLUTE`：一串 SHITEMID 的指针。
    type Pidl = *mut c_void;

    /// 与头文件逐字段对齐；x64 上 `ulFlags` 后面有 4 字节填充，`repr(C)` 会自己补。
    #[repr(C)]
    struct BrowseInfoW {
        hwnd_owner: Hwnd,
        pidl_root: *mut c_void,
        psz_display_name: *mut u16,
        lpsz_title: *const u16,
        ul_flags: u32,
        lpfn: Option<unsafe extern "system" fn(Hwnd, u32, isize, isize) -> i32>,
        l_param: isize,
        i_image: i32,
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHBrowseForFolderW(info: *mut BrowseInfoW) -> Pidl;
        fn SHGetPathFromIDListW(pidl: Pidl, path: *mut u16) -> i32;
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
        fn CoUninitialize();
        fn CoTaskMemFree(ptr: *mut c_void);
    }

    #[link(name = "user32")]
    extern "system" {
        fn SendMessageW(hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize;
    }

    const MAX_PATH: usize = 260;

    const BIF_RETURNONLYFSDIRS: u32 = 0x0000_0001;
    const BIF_EDITBOX: u32 = 0x0000_0010;
    const BIF_NEWDIALOGSTYLE: u32 = 0x0000_0040;

    const BFFM_INITIALIZED: u32 = 1;
    /// `WM_USER + 103`，Unicode 版「把某个目录设为当前选中项」。
    const BFFM_SETSELECTIONW: u32 = 0x0400 + 103;

    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;

    /// 对话框初始化完成后，把初始目录贴进去。
    ///
    /// `wParam` 非 0 表示 `lParam` 是**字符串指针**（传 0 才是 PIDL），所以这里写 1；
    /// `data` 就是调用方放在 `BROWSEINFOW.lParam` 里的那个指针 —— 是**指针**不是索引。
    unsafe extern "system" fn on_initialized(hwnd: Hwnd, msg: u32, _wp: isize, data: isize) -> i32 {
        if msg == BFFM_INITIALIZED && data != 0 {
            SendMessageW(hwnd, BFFM_SETSELECTIONW, 1, data);
        }
        0
    }

    /// 弹一次模态选择框。`owner` 为空（对话框流程没有窗口）时照样能弹。
    /// 初始位置取 `initial` 最接近的已存在目录（见 `nearest_existing_dir`）。
    pub(super) fn pick(initial: &Path, owner: Hwnd) -> PickOutcome {
        // 这块内存必须活到 SHBrowseForFolderW 返回：回调是按指针去读它的。
        let initial_wide = initial_selection_wide(&nearest_existing_dir(initial));
        let title = to_wide_chars(&format!(
            "选择 {APP_NAME} 的安装位置（将在其中创建 {APP_NAME} 文件夹）"
        ));
        let mut info = BrowseInfoW {
            hwnd_owner: owner,
            pidl_root: std::ptr::null_mut(),
            psz_display_name: std::ptr::null_mut(),
            lpsz_title: title.as_ptr(),
            // 只列目录 + 新版对话框（能改大小、能新建文件夹）+ 地址栏（可直接粘路径）。
            ul_flags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX,
            lpfn: Some(on_initialized),
            // 没有初始目录就传 0：回调据此跳过设置选中项，不去解析空串。
            l_param: if initial_wide.is_empty() {
                0
            } else {
                initial_wide.as_ptr() as isize
            },
            i_image: 0,
        };

        // 新版选择框内部要用 shell 的 COM 对象，本线程先按 STA 初始化一次。
        let hr = unsafe { CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED) };
        let co_initialized = hr >= 0;
        // RPC_E_CHANGED_MODE：本线程已被别的套间模型初始化过。这不是错误，对话框照样
        // 能弹；只是这次不能去反初始化（成对规则：谁初始化谁反初始化）。
        if !co_initialized && hr != RPC_E_CHANGED_MODE {
            return PickOutcome::Failed;
        }

        let pidl = unsafe { SHBrowseForFolderW(&mut info) };
        let outcome = if pidl.is_null() {
            // 用户按了取消（或直接关掉框）：保持当前路径不变。
            PickOutcome::Canceled
        } else {
            let mut buffer = [0u16; MAX_PATH];
            let got_path = unsafe { SHGetPathFromIDListW(pidl, buffer.as_mut_ptr()) } != 0;
            // PIDL 是 shell 分配在 COM 任务内存上的，必须还回去。
            unsafe { CoTaskMemFree(pidl) };
            match if got_path {
                wide_to_path(&buffer)
            } else {
                None
            } {
                Some(path) => PickOutcome::Picked(path),
                None => PickOutcome::Failed,
            }
        };

        if co_initialized {
            unsafe { CoUninitialize() };
        }
        outcome
    }
}

/// 让用户挑一个目录。
///
/// `owner` 是安装窗口句柄：非空时选择框**模态于该窗口**（永远在上面，不会被压住）；
/// 对话框流程没有窗口，传空也能用。
///
/// 实现在调用线程上跑 shell32 的模态选择框 —— **不起任何子进程，也没有
/// `.output()`/`wait()` 这类同步等待**；对话框期间消息由 shell 的模态消息泵处理，
/// 所以窗口线程一直在处理消息，Windows 不会把它标成「未响应」。
fn pick_directory(initial: &Path, owner: *mut std::ffi::c_void) -> PickOutcome {
    #[cfg(windows)]
    {
        folder_picker::pick(initial, owner)
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 平台没有 shell32 选择框：保留签名，但没有可视化选择器可用。
        let _ = (initial, owner);
        PickOutcome::Failed
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

/// 安装主体产出的结果。
///
/// 窗口版与对话框版共用同一份安装流程、只由各自的 UI 决定怎么展示；装了什么、
/// 哪些步骤没成功，全部从这里回传——两份实现各自演化必然分叉，所以只留一份。
struct InstallOutcome {
    warnings: Vec<String>,
    target_exe: PathBuf,
    desktop_ok: bool,
    start_menu_ok: bool,
}

/// 创建安装目录。抽成独立函数是为了能在临时目录上单测失败分支，
/// 而单测不必真去写 30MB 主程序、动注册表和桌面。
fn prepare_install_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir)
        .map_err(|e| format!("无法创建安装目录:\n{}\n\n错误: {}", dir.display(), e))
}

/// 安装主体：单窗口版与旧对话框版**共用**这一段。
///
/// `status` 把进度回传给 UI——窗口版写进状态行，对话框版直接丢掉。
/// 只有**致命**失败才返回 `Err`（目录建不出来、主程序写不进去）；快捷方式、
/// 注册表这类非致命步骤失败照旧进 `warnings`，由调用方在完成摘要里如实列出。
fn install_to(dir: &Path, status: &mut dyn FnMut(&str)) -> Result<InstallOutcome, String> {
    // 记录非致命步骤的失败原因，最终在完成提示里如实反馈，避免假装成功。
    let mut warnings: Vec<String> = Vec::new();

    // 1. 创建目标目录
    status("正在创建安装目录…");
    prepare_install_dir(dir)?;

    // 2. 释放主执行程序
    status("正在释放主程序…");
    let target_exe = dir.join(APP_EXE_NAME);
    if MAIN_EXE_BYTES.len() < 1024 * 1024 {
        return Err("安装程序数据不完整，请重新下载官方正式发布的安装包！".to_string());
    }
    if let Err(e) = fs::write(&target_exe, MAIN_EXE_BYTES) {
        return Err(format!(
            "{}\n\n文件: {}\n错误: {}",
            write_failure_hint(&e),
            target_exe.display(),
            e
        ));
    }

    // 3. 复制 tools 目录与 config.toml（如存在）
    status("正在复制组件（tools）…");
    let current_dir = std::env::current_dir().unwrap_or_default();
    let src_config = current_dir.join("config.toml");
    let dst_config = dir.join("config.toml");
    if src_config.exists() && !dst_config.exists() {
        if let Err(e) = fs::copy(&src_config, &dst_config) {
            warnings.push(format!("配置文件 config.toml 复制失败：{e}"));
        }
    }

    let src_tools = current_dir.join("tools");
    let dst_tools = dir.join("tools");
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
    status("正在生成卸载脚本…");
    let uninstall_bat = dir.join("uninstall.bat");
    let bat_content = build_uninstall_script(APP_NAME, APP_EXE_NAME, dir);
    if let Err(e) = fs::write(&uninstall_bat, bat_content) {
        warnings.push(format!("卸载脚本 uninstall.bat 写入失败：{e}"));
    }

    // 5. 创建桌面快捷方式
    status("正在创建快捷方式…");
    let mut desktop_ok = false;
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        let desktop = PathBuf::from(user_profile).join("Desktop");
        if desktop.exists() {
            let desktop_shortcut = desktop.join(format!("{}.lnk", APP_NAME));
            match create_shortcut(&target_exe, &desktop_shortcut, dir) {
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
            match create_shortcut(&target_exe, &start_shortcut, dir) {
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
    status("正在注册卸载项…");
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
        ("InstallLocation", dir.display().to_string()),
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

    Ok(InstallOutcome {
        warnings,
        target_exe,
        desktop_ok,
        start_menu_ok,
    })
}

/// 安装完成摘要（沿用原来的完成提示文案）。
///
/// 窗口版把它放进「复制详情」，对话框版连同一句「是否立即启动」弹出来——文案只有一份。
fn build_finish_summary(dir: &Path, outcome: &InstallOutcome) -> String {
    let mut lines = vec![
        format!("{APP_NAME} v{APP_VERSION} 已成功安装到您的电脑！"),
        String::new(),
        format!("• 安装位置: {}", dir.display()),
    ];
    if outcome.desktop_ok {
        lines.push("• 桌面快捷方式已创建".to_string());
    }
    if outcome.start_menu_ok {
        lines.push("• 开始菜单项已创建".to_string());
    }
    if !outcome.warnings.is_empty() {
        lines.push(String::new());
        lines.push("以下步骤未完全成功，请留意：".to_string());
        for w in outcome.warnings.iter().take(5) {
            lines.push(format!("· {w}"));
        }
        if outcome.warnings.len() > 5 {
            lines.push(format!(
                "· 另有 {} 项提示已省略",
                outcome.warnings.len() - 5
            ));
        }
    }
    lines.join("\n")
}

/// 启动刚装好的主程序；两个入口共用同一份失败文案。
fn launch_installed(dir: &Path, target_exe: &Path) -> Result<(), String> {
    Command::new(target_exe)
        .current_dir(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 {APP_NAME} 失败：\n{e}\n\n您可以稍后从桌面快捷方式手动启动。"))
}

/// 旧的对话框流程：单窗口起不来时（以及非 Windows 平台）走这条，保证「窗口建不出来也还能装」。
fn run_installer_dialog() {
    let default_dir = get_install_dir();
    let install_dir = match select_install_dir(&default_dir) {
        Some(dir) => dir,
        None => return,
    };
    match install_to(&install_dir, &mut |_| {}) {
        Err(message) => {
            message_box("安装失败", &message, MB_OK | MB_ICONERROR);
        }
        Ok(outcome) => {
            let message = format!(
                "{}\n\n是否立即启动 {APP_NAME}？",
                build_finish_summary(&install_dir, &outcome)
            );
            let run_now = message_box(
                &format!("{app} 安装成功", app = APP_NAME),
                &message,
                MB_YESNO | MB_ICONINFORMATION,
            );
            if run_now == IDYES {
                if let Err(message) = launch_installed(&install_dir, &outcome.target_exe) {
                    message_box("启动失败", &message, MB_OK | MB_ICONERROR);
                }
            }
        }
    }
}
/// 单窗口安装器（原生 Win32，不引任何新依赖）。
///
/// # 为什么自己拉窗口
///
/// 原来靠一串 `MessageBoxW` 串成「向导」：欢迎框（是/否/取消）→ 文件夹选择框 →
/// 确认框 → 危险目录再确认 → 完成框再问是否启动，加上控制台黑窗，用户看到的是
/// 「好几个界面」，而且**安装位置从来没在一开始显示出来**。
///
/// 这里用 `CreateWindowExW` + `BUTTON`／`STATIC` 子控件 + 标准消息循环把它们压成
/// 一屏：一上来就显示安装位置，切换路径立刻刷新，装完在同一个窗口里把「安装」
/// 变成「启动 Voice2Word」，全程不弹对话框——只有「目标目录不像本程序的旧安装」
/// 这种边缘情况才确认一次（那是护栏，不是向导步骤）。
#[cfg(windows)]
mod window {
    use super::{
        build_finish_summary, get_install_dir, install_to, is_existing_foreign_dir,
        launch_installed, message_box, pick_directory, resolve_install_dir, to_wide_chars,
        PickOutcome, APP_EXE_NAME, APP_NAME, APP_VERSION, IDYES, MB_ICONERROR, MB_ICONINFORMATION,
        MB_ICONQUESTION, MB_OK, MB_YESNO,
    };
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::process::Command;

    // ---- Win32 基础类型：与文件顶部的 MessageBoxW 同一条路子，手写 extern ----
    type Hwnd = *mut c_void;
    type Hinstance = *mut c_void;
    type Hgdiobj = *mut c_void;
    type Hdc = *mut c_void;
    type Lresult = isize;
    type Wparam = usize;
    type Lparam = isize;

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    /// MSG：只声明到真实尺寸（x64 上 48 字节，含尾部 lPrivate），
    /// 少声明会让 GetMessageW 越界写栈。
    #[repr(C)]
    struct Msg {
        hwnd: Hwnd,
        message: u32,
        w_param: Wparam,
        l_param: Lparam,
        time: u32,
        pt_x: i32,
        pt_y: i32,
        private: u32,
    }

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        wnd_proc: Option<unsafe extern "system" fn(Hwnd, u32, Wparam, Lparam) -> Lresult>,
        cls_extra: i32,
        wnd_extra: i32,
        instance: Hinstance,
        icon: *mut c_void,
        cursor: *mut c_void,
        background: *mut c_void,
        menu_name: *const u16,
        class_name: *const u16,
        icon_sm: *mut c_void,
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassExW(wc: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class_name: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: Hwnd,
            menu: *mut c_void,
            instance: Hinstance,
            param: *mut c_void,
        ) -> Hwnd;
        fn DestroyWindow(hwnd: Hwnd) -> i32;
        fn DefWindowProcW(hwnd: Hwnd, msg: u32, wp: Wparam, lp: Lparam) -> Lresult;
        fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
        fn TranslateMessage(msg: *const Msg) -> i32;
        fn DispatchMessageW(msg: *const Msg) -> Lresult;
        fn PostQuitMessage(code: i32);
        fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
        fn UpdateWindow(hwnd: Hwnd) -> i32;
        fn SetWindowTextW(hwnd: Hwnd, text: *const u16) -> i32;
        fn EnableWindow(hwnd: Hwnd, enable: i32) -> i32;
        fn SendMessageW(hwnd: Hwnd, msg: u32, wp: Wparam, lp: Lparam) -> Lresult;
        fn AdjustWindowRectEx(rect: *mut Rect, style: u32, menu: i32, ex_style: u32) -> i32;
        fn GetSystemMetrics(index: i32) -> i32;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
        fn SetFocus(hwnd: Hwnd) -> Hwnd;
        fn LoadCursorW(instance: Hinstance, name: *const u16) -> *mut c_void;
        fn GetWindowLongPtrW(hwnd: Hwnd, index: i32) -> isize;
        fn SetProcessDPIAware() -> i32;
        fn GetDC(hwnd: Hwnd) -> Hdc;
        fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> Hinstance;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn GetStockObject(kind: i32) -> Hgdiobj;
        fn GetDeviceCaps(hdc: Hdc, index: i32) -> i32;
        fn SetBkMode(hdc: Hdc, mode: i32) -> i32;
        fn SetTextColor(hdc: Hdc, color: u32) -> u32;
        fn SetBkColor(hdc: Hdc, color: u32) -> u32;
    }

    const WINDOW_CLASS: &str = "Voice2WordSetupWindow";

    // 窗口样式：刻意**不含** WS_THICKFRAME / WS_MAXIMIZEBOX —— 固定大小，禁止拉伸。
    const WS_OVERLAPPED: u32 = 0x0000_0000;
    const WS_CAPTION: u32 = 0x00C0_0000;
    const WS_SYSMENU: u32 = 0x0008_0000;
    const WS_MINIMIZEBOX: u32 = 0x0002_0000;
    const WS_CLIPCHILDREN: u32 = 0x0200_0000;
    const WS_CHILD: u32 = 0x4000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_TABSTOP: u32 = 0x0001_0000;
    const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
    const BS_DEFPUSHBUTTON: u32 = 0x0000_0001;
    const SS_LEFT: u32 = 0x0000_0000;
    const SS_LEFTNOWORDWRAP: u32 = 0x0000_000C;
    const SS_PATHELLIPSIS: u32 = 0x0000_8000;

    const SW_HIDE: i32 = 0;
    const SW_SHOW: i32 = 5;
    const SW_SHOWNORMAL: i32 = 1;

    const WM_CREATE: u32 = 0x0001;
    const WM_DESTROY: u32 = 0x0002;
    const WM_CLOSE: u32 = 0x0010;
    const WM_COMMAND: u32 = 0x0111;
    const WM_CTLCOLORSTATIC: u32 = 0x0138;
    const WM_SETFONT: u32 = 0x0030;
    const WM_KEYDOWN: u32 = 0x0100;

    const VK_RETURN: usize = 0x0D;
    const VK_ESCAPE: usize = 0x1B;

    const GWL_EXSTYLE: i32 = -20;
    const DEFAULT_GUI_FONT: i32 = 17;
    const WHITE_BRUSH: i32 = 0;
    /// 系统色 COLOR_BTNFACE(15) 的「+1 即画刷」形式，直接当窗口背景。
    const FACE_BRUSH: isize = 16;
    const LOGPIXELSY: i32 = 90;
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    const IDC_ARROW: usize = 32512;
    const BK_TRANSPARENT: i32 = 1;

    // 逻辑像素下的窗口客户区尺寸（按 DPI 缩放）。
    const CLIENT_W: f32 = 520.0;
    const CLIENT_H: f32 = 196.0;

    const ID_PICK: u16 = 1001;
    const ID_INSTALL: u16 = 1002;
    const ID_CANCEL: u16 = 1003;
    const ID_DETAILS: u16 = 1004;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Phase {
        Idle,
        Installing,
        Done,
        Failed,
    }

    /// 窗口的全部可变状态。放在 thread_local 里而不是 GWLP_USERDATA：
    /// 单线程消息循环只需要一份，RefCell 的借用检查能挡掉「安装回调里再改状态」
    /// 这类重入，不必自己摸裸指针。
    struct AppState {
        dir: PathBuf,
        phase: Phase,
        /// 完成摘要 / 失败原因；「复制详情」和失败弹框都用它。
        details: String,
        target_exe: Option<PathBuf>,
        path_text: Hwnd,
        status: Hwnd,
        pick: Hwnd,
        install: Hwnd,
        cancel: Hwnd,
        details_btn: Hwnd,
    }

    thread_local! {
        static STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };
    }

    fn with_state<R>(f: impl FnOnce(&mut AppState) -> R) -> Option<R> {
        STATE.with(|cell| cell.borrow_mut().as_mut().map(f))
    }

    fn flag(on: bool) -> i32 {
        if on {
            1
        } else {
            0
        }
    }

    fn set_text(control: Hwnd, text: &str) {
        let wide = to_wide_chars(text);
        unsafe {
            SetWindowTextW(control, wide.as_ptr());
        }
    }

    /// 安装是同步跑在 UI 线程上的：状态行写完必须立刻重画，
    /// 否则用户看到的是「卡死」而不是「进度」。
    fn set_status(state: &AppState, text: &str) {
        set_text(state.status, text);
        unsafe {
            UpdateWindow(state.status);
        }
    }

    fn set_path_text(state: &AppState) {
        set_text(state.path_text, &state.dir.display().to_string());
    }

    /// 按阶段统一切换按钮文案／可用状态。集中在一处，避免各分支只改一半。
    fn refresh_buttons(state: &AppState) {
        let (install_text, cancel_text, pick_enabled, main_enabled) = match state.phase {
            Phase::Idle | Phase::Failed => ("安装", "取消", true, true),
            Phase::Installing => ("安装", "取消", false, false),
            Phase::Done => ("启动 Voice2Word", "关闭", false, true),
        };
        let has_details = !state.details.is_empty();
        set_text(state.install, install_text);
        set_text(state.cancel, cancel_text);
        unsafe {
            EnableWindow(state.pick, flag(pick_enabled));
            EnableWindow(state.install, flag(main_enabled));
            // 「取消／关闭」任何时候都能点：装到一半也不会把用户锁死在窗口里。
            EnableWindow(state.cancel, 1);
            EnableWindow(state.details_btn, flag(has_details));
            ShowWindow(
                state.details_btn,
                if has_details { SW_SHOW } else { SW_HIDE },
            );
        }
    }

    fn handle_command(hwnd: Hwnd, id: u16) {
        match id {
            ID_CANCEL => unsafe {
                DestroyWindow(hwnd);
            },
            ID_PICK => pick_dir(hwnd),
            ID_INSTALL => match with_state(|s| s.phase) {
                Some(Phase::Done) => launch_now(hwnd),
                Some(Phase::Idle | Phase::Failed) => run_install(hwnd),
                _ => {}
            },
            ID_DETAILS => copy_and_report(),
            _ => {}
        }
    }

    /// 「选择其他位置…」：以当前值为初始目录，弹原生模态选择框。
    ///
    /// 选择框带 owner（本窗口），模态于本窗口、永远在上面；它跑的是 shell 自己的
    /// 消息泵，所以从点下到选完，窗口线程一直在处理消息，不会「未响应」。
    /// 用户取消 → 保持当前路径不变；真弹不出框才提示一次。
    fn pick_dir(hwnd: Hwnd) {
        if !matches!(with_state(|s| s.phase), Some(Phase::Idle | Phase::Failed)) {
            return;
        }
        let current = match with_state(|s| s.dir.clone()) {
            Some(dir) => dir,
            None => return,
        };
        match pick_directory(&current, hwnd) {
            PickOutcome::Picked(picked) => {
                let resolved = resolve_install_dir(&picked);
                with_state(|s| {
                    s.dir = resolved;
                    set_path_text(s);
                });
            }
            // 取消是正常操作：什么都不做，路径维持原样。
            PickOutcome::Canceled => {}
            // 降级：只提示一次，不改路径，也不退回会卡死窗口的 PowerShell 选目录。
            PickOutcome::Failed => {
                message_box(
                    &format!("{APP_NAME} 安装"),
                    "无法打开系统文件夹选择框，安装位置保持原样。",
                    MB_OK | MB_ICONERROR,
                );
            }
        }
    }

    fn run_install(_hwnd: Hwnd) {
        let dir = match with_state(|s| s.dir.clone()) {
            Some(dir) => dir,
            None => return,
        };
        // 先置忙并置灰按钮：下面的确认框自己会跑消息泵，不设防的话用户能在泵里
        // 再点一次「安装」，叠出第二遍安装流程。
        with_state(|s| {
            s.phase = Phase::Installing;
            refresh_buttons(s);
        });
        // 「目标目录已存在、非空、且不像本程序的旧安装」是护栏（卸载会 rmdir /s /q
        // 整个目录），只在边缘情况确认一次，不算向导步骤。
        if is_existing_foreign_dir(&dir) {
            let proceed = message_box(
                &format!("{app} v{ver} 安装程序", app = APP_NAME, ver = APP_VERSION),
                &format!(
                    "所选目录已存在，且里面没有 {app}（{exe}）：\n{dir}\n\n\
                     它可能是您自己的目录。{app} 只写入自己的文件，但「卸载时会删除\n\
                     整个目录」，其中原有文件也会一并消失，且不可恢复。\n\n\
                     确定仍要安装到这个目录吗？",
                    app = APP_NAME,
                    exe = APP_EXE_NAME,
                    dir = dir.display()
                ),
                MB_YESNO | MB_ICONQUESTION,
            );
            if proceed != IDYES {
                with_state(|s| {
                    s.phase = Phase::Idle;
                    refresh_buttons(s);
                });
                return;
            }
        }

        let status_hwnd = match with_state(|s| s.status) {
            Some(hwnd) => hwnd,
            None => return,
        };
        let mut on_status = |text: &str| {
            let wide = to_wide_chars(text);
            unsafe {
                SetWindowTextW(status_hwnd, wide.as_ptr());
                UpdateWindow(status_hwnd);
            }
        };

        match install_to(&dir, &mut on_status) {
            Ok(outcome) => {
                let summary = build_finish_summary(&dir, &outcome);
                let exe = outcome.target_exe.clone();
                with_state(|s| {
                    s.phase = Phase::Done;
                    s.details = summary;
                    s.target_exe = Some(exe);
                    // 状态与按钮在同一个窗口里就地翻牌：不再弹「是否启动」。
                    set_status(s, "安装完成");
                    refresh_buttons(s);
                });
            }
            Err(message) => {
                with_state(|s| {
                    s.phase = Phase::Failed;
                    s.details = message.clone();
                    set_status(s, "安装失败");
                    refresh_buttons(s);
                });
                // 只在失败时弹一次错误框；正常流程从头到尾不弹，保持「一屏」。
                message_box("安装失败", &message, MB_OK | MB_ICONERROR);
            }
        }
    }

    fn launch_now(hwnd: Hwnd) {
        let (dir, exe) = match with_state(|s| (s.dir.clone(), s.target_exe.clone())) {
            Some((dir, Some(exe))) => (dir, exe),
            _ => return,
        };
        match launch_installed(&dir, &exe) {
            // 起来了就关掉安装窗口：用户接下来看到的就是主程序本身。
            Ok(()) => unsafe {
                DestroyWindow(hwnd);
            },
            Err(message) => {
                message_box("启动失败", &message, MB_OK | MB_ICONERROR);
            }
        }
    }

    /// 「复制详情」：走临时文件 + Set-Clipboard。
    /// 多行文本直接内插进命令行会被引号/换行拆坏，和注册表那条经验一样，绕开解析层。
    fn copy_details(state: &AppState) -> Result<(), String> {
        let path =
            std::env::temp_dir().join(format!("voice2word_setup_{}.txt", std::process::id()));
        std::fs::write(&path, &state.details).map_err(|e| e.to_string())?;
        let quoted = path.to_string_lossy().replace('\'', "''");
        let script = format!(
            "Set-Clipboard -Value ([IO.File]::ReadAllText('{quoted}', [Text.Encoding]::UTF8))"
        );
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &script,
            ])
            .output()
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&path);
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    fn copy_and_report() {
        let details = match with_state(|s| s.details.clone()) {
            Some(text) => text,
            None => return,
        };
        if details.is_empty() {
            return;
        }
        if let Some(Err(e)) = with_state(|s| copy_details(s)) {
            message_box(
                &format!("{APP_NAME} 安装详情"),
                &format!("复制到剪贴板失败：{e}\n\n{details}"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
    /// 子控件规格：逻辑坐标在调用处按 DPI 换算好，避免每个控件各自算一遍。
    struct Ctrl<'a> {
        class: &'a str,
        text: &'a str,
        style: u32,
        ex_style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: u16,
    }

    fn create_child(parent: Hwnd, instance: Hinstance, font: Hgdiobj, spec: &Ctrl<'_>) -> Hwnd {
        let class = to_wide_chars(spec.class);
        let text = to_wide_chars(spec.text);
        let control = unsafe {
            CreateWindowExW(
                spec.ex_style,
                class.as_ptr(),
                text.as_ptr(),
                spec.style,
                spec.x,
                spec.y,
                spec.w,
                spec.h,
                parent,
                spec.id as usize as *mut c_void,
                instance,
                std::ptr::null_mut(),
            )
        };
        if !control.is_null() {
            // DEFAULT_GUI_FONT 而不是系统默认字体：后者在中文界面下又小又糊。
            unsafe {
                SendMessageW(control, WM_SETFONT, font as Wparam, 1);
            }
        }
        control
    }

    /// 一次把整屏控件建出来；任何控件建失败都返回 false，让调用方退回对话框流程。
    fn on_create(hwnd: Hwnd) -> bool {
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let scale = dpi_scale();
        let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
        let button_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP;
        let label_style = WS_CHILD | WS_VISIBLE;
        let at = |x: f32, y: f32, w: f32, h: f32| {
            (px(x, scale), px(y, scale), px(w, scale), px(h, scale))
        };

        let title_text = format!("{APP_NAME} 安装");
        let hint_text = "选择安装位置，然后点【安装】。".to_string();
        let path_label = "安装位置：".to_string();
        let (tx, ty, tw, th) = at(16.0, 14.0, 488.0, 20.0);
        let title = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "STATIC",
                text: &title_text,
                style: label_style | SS_LEFT,
                ex_style: 0,
                x: tx,
                y: ty,
                w: tw,
                h: th,
                id: 0,
            },
        );
        let (hx, hy, hw, hh) = at(16.0, 36.0, 488.0, 18.0);
        let hint = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "STATIC",
                text: &hint_text,
                style: label_style | SS_LEFT,
                ex_style: 0,
                x: hx,
                y: hy,
                w: hw,
                h: hh,
                id: 0,
            },
        );
        let (lx, ly, lw, lh) = at(16.0, 68.0, 64.0, 18.0);
        let label = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "STATIC",
                text: &path_label,
                style: label_style | SS_LEFT,
                ex_style: 0,
                x: lx,
                y: ly,
                w: lw,
                h: lh,
                id: 0,
            },
        );
        // 安装位置：只读文本 + SS_PATHELLIPSIS —— 长路径压缩中间段，
        // 用户一眼能看出装到哪个盘、哪个目录，而不是只看得到开头。
        let (px_, py_, pw_, ph_) = at(84.0, 66.0, 420.0, 22.0);
        let path_text = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "STATIC",
                text: "",
                style: label_style | SS_LEFTNOWORDWRAP | SS_PATHELLIPSIS,
                ex_style: WS_EX_CLIENTEDGE,
                x: px_,
                y: py_,
                w: pw_,
                h: ph_,
                id: 0,
            },
        );
        let (sx, sy, sw, sh) = at(16.0, 104.0, 356.0, 18.0);
        let status = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "STATIC",
                text: "",
                style: label_style | SS_LEFT,
                ex_style: 0,
                x: sx,
                y: sy,
                w: sw,
                h: sh,
                id: 0,
            },
        );
        let (dx, dy, dw, dh) = at(384.0, 100.0, 120.0, 26.0);
        let details_btn = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "BUTTON",
                text: "复制详情",
                style: button_style,
                ex_style: 0,
                x: dx,
                y: dy,
                w: dw,
                h: dh,
                id: ID_DETAILS,
            },
        );
        let (cx, cy, cw, ch) = at(16.0, 150.0, 168.0, 30.0);
        let pick = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "BUTTON",
                text: "选择其他位置…",
                style: button_style,
                ex_style: 0,
                x: cx,
                y: cy,
                w: cw,
                h: ch,
                id: ID_PICK,
            },
        );
        let (ix, iy, iw, ih) = at(264.0, 150.0, 120.0, 30.0);
        let install = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "BUTTON",
                text: "安装",
                // 默认按钮：Enter 由消息循环转发成主按钮点击。
                style: button_style | BS_DEFPUSHBUTTON,
                ex_style: 0,
                x: ix,
                y: iy,
                w: iw,
                h: ih,
                id: ID_INSTALL,
            },
        );
        let (nx, ny, nw, nh) = at(392.0, 150.0, 112.0, 30.0);
        let cancel = create_child(
            hwnd,
            instance,
            font,
            &Ctrl {
                class: "BUTTON",
                text: "取消",
                style: button_style,
                ex_style: 0,
                x: nx,
                y: ny,
                w: nw,
                h: nh,
                id: ID_CANCEL,
            },
        );

        if [
            title,
            hint,
            label,
            path_text,
            status,
            details_btn,
            pick,
            install,
            cancel,
        ]
        .iter()
        .any(|h| h.is_null())
        {
            return false;
        }

        let state = AppState {
            dir: get_install_dir(),
            phase: Phase::Idle,
            details: String::new(),
            target_exe: None,
            path_text,
            status,
            pick,
            install,
            cancel,
            details_btn,
        };
        set_path_text(&state);
        set_status(&state, "准备就绪");
        // 「复制详情」只在失败/完成后才有内容，平时不占视线。
        unsafe {
            ShowWindow(details_btn, SW_HIDE);
        }
        STATE.with(|cell| {
            *cell.borrow_mut() = Some(state);
        });
        true
    }

    /// STATIC 的底色：普通文字直接落在窗口底色上（不然会出现白底方块）；
    /// 只有带 WS_EX_CLIENTEDGE 的「安装位置」那一行按输入框画成白底。
    fn on_ctl_color(wp: Wparam, lp: Lparam) -> Lresult {
        let hdc = wp as Hdc;
        let control = lp as Hwnd;
        unsafe {
            SetBkMode(hdc, BK_TRANSPARENT);
            if GetWindowLongPtrW(control, GWL_EXSTYLE) & (WS_EX_CLIENTEDGE as isize) != 0 {
                SetBkColor(hdc, 0x00FF_FFFF);
                SetTextColor(hdc, 0x0000_0000);
                return GetStockObject(WHITE_BRUSH) as Lresult;
            }
        }
        FACE_BRUSH
    }

    fn dpi_scale() -> f32 {
        unsafe {
            let hdc = GetDC(std::ptr::null_mut());
            if hdc.is_null() {
                return 1.0;
            }
            let dpi = GetDeviceCaps(hdc, LOGPIXELSY);
            ReleaseDC(std::ptr::null_mut(), hdc);
            if dpi > 0 {
                dpi as f32 / 96.0
            } else {
                1.0
            }
        }
    }

    fn px(value: f32, scale: f32) -> i32 {
        (value * scale).round() as i32
    }

    unsafe extern "system" fn wnd_proc(hwnd: Hwnd, msg: u32, wp: Wparam, lp: Lparam) -> Lresult {
        match msg {
            // 控件建立失败就让 CreateWindowExW 整体失败 —— 不半残地显示一个空窗口。
            WM_CREATE => {
                if on_create(hwnd) {
                    0
                } else {
                    -1
                }
            }
            WM_COMMAND => {
                let id = (wp & 0xFFFF) as u16;
                if id != 0 {
                    handle_command(hwnd, id);
                }
                0
            }
            WM_CTLCOLORSTATIC => on_ctl_color(wp, lp),
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                STATE.with(|cell| {
                    cell.borrow_mut().take();
                });
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }

    /// 跑单窗口安装器。返回 false 表示窗口建不起来（例如 CreateWindowExW 返回空），
    /// 调用方必须退回对话框流程 —— 「窗口起不来也还能装」。
    pub(crate) fn run() -> bool {
        unsafe {
            // 高分屏下不加这句，窗口会被系统拉伸放大成一个糊块。
            SetProcessDPIAware();
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name = to_wide_chars(WINDOW_CLASS);
            let cursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16);
            let wc = WndClassExW {
                cb_size: std::mem::size_of::<WndClassExW>() as u32,
                style: 0,
                wnd_proc: Some(wnd_proc),
                cls_extra: 0,
                wnd_extra: 0,
                instance,
                icon: std::ptr::null_mut(),
                cursor,
                background: FACE_BRUSH as *mut c_void,
                menu_name: std::ptr::null(),
                class_name: class_name.as_ptr(),
                icon_sm: std::ptr::null_mut(),
            };
            if RegisterClassExW(&wc) == 0 {
                return false;
            }

            let scale = dpi_scale();
            // 固定大小：样式里没有 WS_THICKFRAME，再把客户区反算成窗口大小。
            let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
            let mut rect = Rect {
                left: 0,
                top: 0,
                right: px(CLIENT_W, scale),
                bottom: px(CLIENT_H, scale),
            };
            AdjustWindowRectEx(&mut rect, style, 0, 0);
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            let x = ((GetSystemMetrics(SM_CXSCREEN) - width) / 2).max(0);
            let y = ((GetSystemMetrics(SM_CYSCREEN) - height) / 2).max(0);
            let caption = to_wide_chars(&format!(
                "{app} v{ver} 安装",
                app = APP_NAME,
                ver = APP_VERSION
            ));
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                caption.as_ptr(),
                style,
                x,
                y,
                width,
                height,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                STATE.with(|cell| {
                    cell.borrow_mut().take();
                });
                return false;
            }

            ShowWindow(hwnd, SW_SHOWNORMAL);
            UpdateWindow(hwnd);
            SetForegroundWindow(hwnd);
            if let Some(focus) = with_state(|s| s.install) {
                SetFocus(focus);
            }

            let mut msg: Msg = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if msg.message == WM_KEYDOWN {
                    // 没有对话框管理器，Enter/Esc 得自己接：Enter 当主按钮，Esc 关窗。
                    if msg.w_param == VK_RETURN {
                        SendMessageW(hwnd, WM_COMMAND, ID_INSTALL as Wparam, 0);
                        continue;
                    }
                    if msg.w_param == VK_ESCAPE {
                        DestroyWindow(hwnd);
                        continue;
                    }
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            true
        }
    }
}

fn main() {
    #[cfg(windows)]
    {
        // 单窗口安装器；窗口创建失败就退回原来的对话框流程，保证还能装。
        if window::run() {
            return;
        }
    }
    run_installer_dialog();
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

    /// 初始路径转宽字符：必须 NUL 结尾（`SHBrowseForFolderW` 按指针一直读到 NUL），
    /// 且没有初始目录时返回空 —— 调用方据此不设置选中项，免得去解析空串。
    #[test]
    fn initial_selection_wide_is_nul_terminated_and_empty_when_missing() {
        let wide = initial_selection_wide(Path::new(r"D:\Apps"));
        let expected: Vec<u16> = r"D:\Apps"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        assert_eq!(wide, expected);
        assert_eq!(*wide.last().unwrap(), 0);
        assert!(initial_selection_wide(Path::new("")).is_empty());
    }

    /// 初始目录要落在**已存在**的目录上：默认安装目录 `...\Programs\Voice2Word` 在装
    /// 之前不存在，shell 会把这种初始选中静默丢掉（对话框退回默认位置）。
    #[test]
    fn nearest_existing_dir_walks_up_to_an_existing_ancestor() {
        let base = std::env::temp_dir();
        // 已存在的目录：原样返回。
        assert_eq!(nearest_existing_dir(&base), base);
        // 不存在的多级子目录：回到最近的存在祖先（临时目录本身）。
        let missing = base
            .join("v2w_setup_anchor_missing")
            .join("Voice2Word")
            .join("sub");
        assert_eq!(nearest_existing_dir(&missing), base);
    }

    /// PIDL 回填的缓冲转路径：按第一个 NUL 截断，尾部残留的 0 不算路径内容；
    /// 截断后为空（没拿到路径）必须返回 None，而不是一个空 PathBuf。
    #[test]
    fn wide_to_path_stops_at_nul_and_rejects_empty() {
        let mut buffer = [0u16; 32];
        for (slot, unit) in buffer.iter_mut().zip(r"C:\Apps\Voice2Word".encode_utf16()) {
            *slot = unit;
        }
        assert_eq!(
            wide_to_path(&buffer),
            Some(PathBuf::from(r"C:\Apps\Voice2Word"))
        );
        assert_eq!(wide_to_path(&[0u16; 32]), None);
        assert_eq!(wide_to_path(&[]), None);
    }
}

/// 安装器健壮性回归：路径转义与「写入被占用」的提示。
///
/// 刻意与上面的向导测试分开、且不 `use super::*`：只显式引入被测函数，
/// 这样新增的依赖不会悄悄把整份模块塞进来。
#[cfg(test)]
mod robustness_tests {
    use super::{
        build_uninstall_script, escape_batch_percent, is_existing_foreign_dir, prepare_install_dir,
        write_failure_hint,
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

    #[test]
    fn prepare_install_dir_creates_missing_target() {
        // 装到全新位置是最常见的路径：目标目录不存在时必须自己建出来
        let dir = std::env::temp_dir().join(format!("v2w_setup_new_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("a").join("b");
        assert!(prepare_install_dir(&nested).is_ok());
        assert!(nested.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_install_dir_returns_err_when_creation_fails() {
        // 父级是文件 → 建目录必然失败。必须返回 Err 让 UI 把话说清楚，而不是 panic：
        // 单窗口版就靠这个 Err 把状态行翻成「安装失败」。
        let blocker = std::env::temp_dir().join(format!("v2w_setup_block_{}", std::process::id()));
        let _ = std::fs::remove_file(&blocker);
        let _ = std::fs::remove_dir_all(&blocker);
        std::fs::write(&blocker, b"not a dir").unwrap();
        let err = prepare_install_dir(&blocker.join("sub")).unwrap_err();
        assert!(err.contains("无法创建安装目录"), "{err}");
        let _ = std::fs::remove_file(&blocker);
    }
}
