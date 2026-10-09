//! 启动体检 / 故障自述报告：把「程序实际解析到什么」一次性摆给用户看。
//!
//! # 为什么需要这个模块
//!
//! 项目里几乎所有失败信息都只指向**某个配置字段**（「ffmpeg 未就绪」
//! 「whisper-cli 无法启动」「数据库打开失败」），但用户看不到程序**实际解析出
//! 的值**：
//!
//! - 锚定的项目根是哪个目录（`models/` 判据命中了吗）；
//! - 每个二进制在不在、有多大；
//! - whisper-cli / llama.cpp 的运行时 DLL 能不能被加载器解析到；
//! - 数据库文件在不在、WAL 伴随文件是否正常；
//! - 项目根到底能不能写（不能写就是启动时一个裸 `?` 直接退出）。
//!
//! 这些信息此前只散落在若干条日志里，于是「它说 ffmpeg 缺失，可我有 ffmpeg」
//! 这类对话要来回好几天才能定位。本模块把它们汇总成**一份自包含、可整段复制的
//! 文本报告**：用户只要把报告贴出来，路径对不对、缺哪个 DLL 一目了然。
//!
//! # 设计约束
//!
//! - **纯数据采集 + 格式化**：不依赖 UI、不联网、不弹窗；`build_report` 是
//!   `(配置, 项目根) -> String` 的纯函数（只额外读磁盘元数据）。
//! - **绝不 panic**：文件不存在、目录不可读、exe 不是 PE、根目录本身不存在……
//!   全部退化成一条 `Fail` / `Warn` 检查项。报告是给「已经坏了」的场景用的，
//!   它自己再崩一次毫无意义。
//! - **确定性**：同样输入必须得到逐字节相同的输出（没有时间戳、没有随机数），
//!   这样测试才能断言固定文本，用户也能两版报告直接 diff。
//! - **不含密钥**：报告刻意**不读也不打印** `translate.api_key` /
//!   `effective_api_key()`（见 `run_checks` 内的注释与 `report_omits_api_key`
//!   测试）。报告是要贴进聊天窗口 / 邮件里的，带密钥等于泄露。
//!
//! # 为什么没有「磁盘剩余空间」这一项
//!
//! `build_report` 的契约是「同样输入 → 逐字节相同输出」，而剩余空间随任何一次
//! 写入实时变化，一旦写进报告就立刻破坏这条契约（测试无法断言，两版报告也无法
//! diff），拿它还要多做一次系统调用。实时容量显示属于界面层的监控
//! （`utils::monitor`），这里不重复，因此**不**为它留占位项。
//!
//! # 关于路径解析
//!
//! 所有相对路径都以调用方传入的 `root`（即报告里的项目根）为锚点展开，
//! **不**去问 `AppConfig::app_root_dir()`——那个函数依赖进程 cwd / 环境变量，
//! 会让报告不再是「输入的纯函数」，也会把开发机的真实路径泄进报告里。

use std::path::{Path, PathBuf};

use super::model_download::{self, DownloadItem, ITEMS};
use super::pe_imports;
use crate::utils::config::AppConfig;

/// 报告里的一个检查项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckItem {
    /// 稳定标识（写进报告，便于用户引用）
    pub id: &'static str,
    /// 人类可读名称
    pub label: String,
    /// 三态结论
    pub status: CheckStatus,
    /// 详情：实际路径 / 版本 / 错误原因
    pub detail: String,
}

/// 一个检查项的三态结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// 一切正常
    Ok,
    /// 能用，但有值得注意的地方（例如回落、体积偏小、只读）
    Warn,
    /// 不可用（缺失 / 无法读取）
    Fail,
}

impl CheckStatus {
    /// `"[OK]"` / `"[WARN]"` / `"[FAIL]"`——定宽，报告里能对齐。
    ///
    /// 定宽是为了让用户一眼扫出「哪几项红了」：三态前缀长度固定，前缀后面的
    /// 标签就自然对齐成一列；一旦哪天改成 `"OK"` 之类不带方括号的形式，
    /// 整份报告的对齐就散了。
    pub fn tag(self) -> &'static str {
        match self {
            Self::Ok => "[OK]",
            Self::Warn => "[WARN]",
            Self::Fail => "[FAIL]",
        }
    }
}

/// 生成完整报告（纯函数：输入配置 + 项目根，输出文本）。
///
/// 输出是**纯文本**：没有 ANSI 颜色、没有控制序列——用户要把它贴进聊天窗口或
/// 邮件里，任何终端控制字符都会变成乱码噪声。
pub fn build_report(cfg: &crate::utils::config::AppConfig, root: &std::path::Path) -> String {
    let items = run_checks(cfg, root);
    let mut out = String::new();
    out.push_str("Voice2Word 诊断报告\n");
    out.push_str(&"=".repeat(64));
    out.push('\n');
    out.push_str("用途：出现「组件缺失 / 路径不对 / 启动即失败」时，把本报告整段复制出来。\n");
    // 明写「不含密钥」，避免用户误以为报告里已经带了密钥而不敢外发。
    out.push_str("说明：本报告刻意不包含任何密钥或令牌。\n");
    out.push_str(&"-".repeat(64));
    out.push('\n');
    for item in &items {
        out.push_str(item.status.tag());
        out.push(' ');
        out.push_str(&item.label);
        out.push_str(": ");
        out.push_str(&item.detail);
        out.push('\n');
        // 只有非 Ok 的项才给建议：正常项配一行建议只会淹没有用信息。
        if let Some(hint) = hint_for(item.id, item.status) {
            out.push_str("     建议：");
            out.push_str(hint);
            out.push('\n');
        }
    }
    out.push_str(&"-".repeat(64));
    out.push('\n');
    let (ok, warn, fail) = count_statuses(&items);
    out.push_str(&format!(
        "汇总：正常 {ok} 项 / 注意 {warn} 项 / 失败 {fail} 项\n"
    ));
    out
}

/// 逐项检查（供界面 / 单测消费，`build_report` 内部也用它）。
///
/// 返回顺序即报告顺序，**已固定**：项目根在最前（它是所有相对路径的锚点，
/// 根错了下面每一项都会跟着错，先看它最省时间），其余按「组件 → 模型 → 环境
/// → 数据」排列。单测 `check_ids_are_unique_and_ordered` 把顺序钉死。
///
/// # 为什么没有「密钥」这一项
///
/// 报告会贴给他人，因此这里**只**检查路径、存在性、可写性与运行库依赖；
/// 刻意**不**读取 `cfg.translate.api_key`，也**不**调用 `effective_api_key()`
/// （后者会把环境变量里的密钥取出来）——本模块对密钥零接触。
pub fn run_checks(cfg: &crate::utils::config::AppConfig, root: &std::path::Path) -> Vec<CheckItem> {
    vec![
        check_project_root(root),
        check_ffmpeg(cfg, root),
        check_binary("whisper_cli", "whisper-cli", &cfg.paths.whisper_cli, root),
        check_whisper_model(cfg, root),
        check_binary("llama_cpp", "llama.cpp", &cfg.paths.llama_cli, root),
        check_llm_model(cfg, root),
        check_python(cfg, root),
        check_database(root),
        check_config_files(root),
        check_data_dir_writable(root),
    ]
}
// ───────────────────────── 各项检查 ─────────────────────────

/// 项目根：报告里的第一条，也是其余所有相对路径的锚点。
///
/// 判据与 `AppConfig` 一致——根下存在 `models/` 才算「找对了地方」。
/// 根不存在 → `Fail`；根在但 `models/` 不在 → `Warn`（首次运行还没下模型，
/// 这本身不致命，但足以解释「为什么所有模型都显示缺失」）。
fn check_project_root(root: &Path) -> CheckItem {
    let (status, detail) = if !root.is_dir() {
        (
            CheckStatus::Fail,
            format!("项目根 {} 不存在", root.display()),
        )
    } else if root.join("models").is_dir() {
        (
            CheckStatus::Ok,
            format!("项目根 {}（models/ 存在）", root.display()),
        )
    } else {
        (
            CheckStatus::Warn,
            format!("项目根 {}（根下没有 models/ 目录）", root.display()),
        )
    };
    CheckItem {
        id: "project_root",
        label: "项目根".to_string(),
        status,
        detail,
    }
}

/// FFmpeg：唯一**没有回退**的组件，所以用 `is_file()` 而不是 `exists()` 判定。
///
/// 为什么强调 `is_file`：同名的**目录**会让 `exists()` 通过，而真正调用 ffmpeg
/// 时只会得到一个含糊的「不是有效的 Win32 应用程序」。体检必须在用户之前发现它。
fn check_ffmpeg(cfg: &AppConfig, root: &Path) -> CheckItem {
    let path = resolve_under(root, &cfg.paths.ffmpeg);
    let (status, detail) = if path.is_file() {
        (CheckStatus::Ok, describe_file(&path))
    } else {
        (CheckStatus::Fail, format!("{} 不存在", path.display()))
    };
    CheckItem {
        id: "ffmpeg",
        label: "FFmpeg".to_string(),
        status,
        detail,
    }
}

/// 二进制组件：存在性 + **导入表可解析性**。
///
/// # 为什么「文件在」还不够
///
/// whisper-cli / llama.cpp 的官方包是「几 KB 的启动桩 + 同目录一堆 DLL」。
/// 只判断文件在不在，会把「解压了一半（有 exe 没 DLL）」和「MinGW 构建缺
/// `libgcc_s_seh-1.dll`」这类**文件明明在却起不来**的情况判成正常——这正是
/// 本项目已经踩过两次的坑。所以这里进一步用 `pe_imports::missing_imports`
/// 沿同目录 DLL 递归展开导入闭包，报告哪些 DLL 加载器找不到
/// （判定范围：exe 自己的目录 + `System32`，与 `pe_imports` 内部一致）。
///
/// 解析不了 PE（不是可执行文件）时不误报「运行库缺失」：退回 `Ok` 并在详情里
/// 说明「导入表无法解析」，把判断权交给用户。
fn check_binary(id: &'static str, label: &str, raw: &str, root: &Path) -> CheckItem {
    let path = resolve_under(root, raw);
    let mut item = CheckItem {
        id,
        label: label.to_string(),
        status: CheckStatus::Ok,
        detail: String::new(),
    };
    if !path.is_file() {
        item.status = CheckStatus::Fail;
        item.detail = format!("{} 不存在", path.display());
        return item;
    }
    // 先自己读一遍：读不到（被占用 / 权限）与「读到了但不是 PE」是两种不同情况，
    // 混在一起会让用户以为是缺 DLL。
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) => {
            item.status = CheckStatus::Warn;
            item.detail = format!("{}（存在但读取失败：{err}）", path.display());
            return item;
        }
    };
    if pe_imports::imported_dll_names(&bytes).is_none() {
        item.detail = format!(
            "{}（导入表无法解析：可能不是 PE 可执行文件）",
            describe_file(&path)
        );
        return item;
    }
    let missing = pe_imports::missing_imports(&path);
    if missing.is_empty() {
        item.detail = format!("{}，导入的 DLL 均可解析", describe_file(&path));
    } else {
        item.status = CheckStatus::Warn;
        item.detail = format!(
            "{}，以下导入 DLL 无法解析：{}",
            describe_file(&path),
            missing.join("、")
        );
    }
    item
}

/// Whisper 模型：存在性 + 体积下限。
///
/// # 为什么要比体积
///
/// 下载中断会留下一个**体积明显偏小**的文件（本项目用 `.part` 临时名规避，
/// 但用户自己拷来的、或旧版本留下的截断文件仍在）。截断的 ggml 文件要到加载时
/// 才炸，且报错完全看不出是体积问题。这里复用下载清单里登记的 `min_size`
/// （见 `model_download::ITEMS`），小于下限即 `Warn`。
///
/// 文件名对不上任何条目时**跳过**体积比较：用户完全可能用自定义量化版，
/// 拿别人的下限去卡他只会误报。
fn check_whisper_model(cfg: &AppConfig, root: &Path) -> CheckItem {
    let path = resolve_under(root, &cfg.paths.whisper_model);
    let mut item = CheckItem {
        id: "whisper_model",
        label: "Whisper 模型".to_string(),
        status: CheckStatus::Ok,
        detail: String::new(),
    };
    if !path.is_file() {
        item.status = CheckStatus::Fail;
        item.detail = format!("{} 不存在", path.display());
        return item;
    }
    let size = model_download::disk_size(&path).unwrap_or(0);
    match matching_item(&path) {
        Some(entry) => {
            let floor = effective_min_size(entry);
            if floor > 0 && size < floor {
                item.status = CheckStatus::Warn;
                item.detail = format!(
                    "{}（{}，小于最小完整体积 {}，疑似下载被截断）",
                    path.display(),
                    model_download::human_size(size),
                    model_download::human_size(floor)
                );
            } else {
                item.detail = describe_file(&path);
            }
        }
        // 清单里没有对应条目：只报体积，不做体积下限判断。
        None => item.detail = describe_file(&path),
    }
    item
}

/// LLM 模型：存在性 + 体积 + 扩展名。
///
/// 扩展名单独拎出来报，是因为 llama.cpp **按扩展名拒绝**非 `.gguf` 文件，
/// 而用户手上常见的错法是拿到 `.bin` / `.onnx` / 忘了改名的下载文件：
/// 报错信息只说「无法加载」，看不出是扩展名的问题。
fn check_llm_model(cfg: &AppConfig, root: &Path) -> CheckItem {
    let path = resolve_under(root, &cfg.paths.llm_model);
    let mut item = CheckItem {
        id: "llm_model",
        label: "LLM 模型".to_string(),
        status: CheckStatus::Ok,
        detail: String::new(),
    };
    if !path.is_file() {
        item.status = CheckStatus::Fail;
        item.detail = format!("{} 不存在", path.display());
        return item;
    }
    let is_gguf = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf"));
    if is_gguf {
        item.detail = describe_file(&path);
    } else {
        item.status = CheckStatus::Warn;
        item.detail = format!(
            "{}（扩展名不是 .gguf，llama.cpp 会拒绝加载）",
            describe_file(&path)
        );
    }
    item
}

/// Python 解释器：裸命令名与显式路径分开报。
///
/// 裸命令名（如默认的 `python`）走系统 `PATH` 解析。这里**不去**遍历 `PATH`：
/// 逐目录 stat 既慢，又容易把「恰好同名的无关文件」当成可用解释器，报告里给出
/// 一个假的「存在」比不报更糟。因此裸命令名只如实说明「交由 PATH 解析，
/// 本报告不解析 PATH」，**不**判成缺失。只有「写成了路径形式却不存在」
/// 才是 `Warn`。
fn check_python(cfg: &AppConfig, root: &Path) -> CheckItem {
    let raw = cfg.paths.python.trim();
    let mut item = CheckItem {
        id: "python",
        label: "Python".to_string(),
        status: CheckStatus::Ok,
        detail: String::new(),
    };
    if !raw.contains('/') && !raw.contains('\\') {
        item.detail =
            format!("{raw}（裸命令名，交由系统 PATH 解析；本报告不解析 PATH，可用性未在此判定）");
        return item;
    }
    let path = resolve_under(root, raw);
    if path.is_file() {
        item.detail = describe_file(&path);
    } else {
        item.status = CheckStatus::Warn;
        item.detail = format!("{}（按路径配置，但该文件不存在）", path.display());
    }
    item
}

/// 数据库：`<root>/voice2word.db`。
///
/// 缺失只是 `Warn` 而非 `Fail`：首次运行本来就会创建它（`main` 里
/// `Database::open` 顺带建表），把「还没跑过」报成失败会吓人。
/// 存在时顺带说明 `-wal` 伴随文件——WAL 模式下有它是**正常**的，
/// 用户看到多出一个文件常常会以为出了问题。
fn check_database(root: &Path) -> CheckItem {
    let path = root.join("voice2word.db");
    let mut item = CheckItem {
        id: "database",
        label: "数据库".to_string(),
        status: CheckStatus::Ok,
        detail: String::new(),
    };
    if !path.is_file() {
        item.status = CheckStatus::Warn;
        item.detail = format!("{} 不存在（首次运行时自动创建）", path.display());
        return item;
    }
    let wal_note = if sidecar_path(&path, "-wal").is_file() {
        "，-wal 伴随文件存在（WAL 模式下正常）"
    } else {
        "，无 -wal 伴随文件"
    };
    item.detail = format!("{}{wal_note}", describe_file(&path));
    item
}

/// 配置文件：`config.toml`（共享）与 `config.local.toml`（本机覆盖）。
///
/// `config.local.toml` 不存在是**便携默认**，绝不能因此判 Fail——CI、新机器、
/// 只用共享配置的用户都是这个状态。只有 `config.toml` 缺失才 `Fail`。
fn check_config_files(root: &Path) -> CheckItem {
    let shared = root.join("config.toml");
    let local = root.join(AppConfig::LOCAL_OVERRIDE);
    let mut item = CheckItem {
        id: "config_files",
        label: "配置文件".to_string(),
        status: CheckStatus::Ok,
        detail: String::new(),
    };
    if !shared.is_file() {
        item.status = CheckStatus::Fail;
        item.detail = "config.toml 不存在".to_string();
        return item;
    }
    item.detail = if local.is_file() {
        "config.toml 存在，config.local.toml 存在".to_string()
    } else {
        "config.toml 存在，config.local.toml 不存在（便携默认，正常）".to_string()
    };
    item
}

/// 项目根可写性：真正写一个临时文件再删掉。
///
/// # 为什么值得单独一项
///
/// 项目根不可写时，启动路径上第一个 `?` 就会把整个应用带走，用户只看到一句
/// 「拒绝访问」，完全不知道说的是哪个目录。这里直接实测一次写入，把「根目录 +
/// 失败原因」明确报出来。探针文件用完即删，**绝不留残留**。
fn check_data_dir_writable(root: &Path) -> CheckItem {
    let mut item = CheckItem {
        id: "data_dir_writable",
        label: "项目根可写性".to_string(),
        status: CheckStatus::Ok,
        detail: String::new(),
    };
    match probe_writable(root) {
        Ok(()) => item.detail = format!("{}（写入并删除临时文件成功）", root.display()),
        Err(err) => {
            item.status = CheckStatus::Fail;
            item.detail = format!("{}（不可写：{err}）", root.display());
        }
    }
    item
}

// ───────────────────────── 内部工具 ─────────────────────────

/// 相对路径以 `root` 为锚点展开；绝对路径原样返回。
fn resolve_under(root: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// `<file>-wal` / `<file>-shm` 这类 SQLite 伴随文件路径。
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(suffix);
    PathBuf::from(text)
}

/// 路径 + 体积（读不到元数据时只给路径，绝不 panic）。
fn describe_file(path: &Path) -> String {
    match model_download::disk_size(path) {
        Some(bytes) => format!(
            "{}（{}）",
            path.display(),
            model_download::human_size(bytes)
        ),
        None => path.display().to_string(),
    }
}

/// 清单里某个条目 `dest` 的文件名部分（小写）。
fn dest_basename(item: &DownloadItem) -> String {
    item.dest
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// 由磁盘上的文件名反查它属于哪个下载条目（可能没有）。
///
/// 只按**文件名**匹配，不按完整路径：用户可以把 `config.local.toml` 里的
/// `paths.whisper_model` 指到任何目录（例如自己整理的模型盘），按路径匹配就会
/// 漏掉他手上那个完好的文件。文件名一致即可认定是同一档模型，与
/// `model_download::item_id_for_path` 同属一个身份空间。
fn matching_item(path: &Path) -> Option<&'static DownloadItem> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if let Some(entry) = ITEMS.iter().find(|i| dest_basename(i) == name) {
        return model_download::item_by_id(entry.id);
    }
    // whisper 档位内的文件名互换：上游是 `q5_1`、本项目本地量化是 `q5_0`，
    // 两者是**同一档**的两种合法形态（规则与 `model_download` 内的档位判定一致）。
    ITEMS
        .iter()
        .find(|i| same_whisper_tier(i, &name))
        .and_then(|i| model_download::item_by_id(i.id))
}

/// 两个文件名是否属于同一个 whisper 档位（small / base / turbo-q5 / turbo-q8）。
fn same_whisper_tier(item: &DownloadItem, name: &str) -> bool {
    let dest = dest_basename(item);
    (dest.starts_with("ggml-small") && name.starts_with("ggml-small"))
        || (dest.starts_with("ggml-base") && name.starts_with("ggml-base"))
        || (dest.contains("turbo-q5") && name.contains("turbo-q5"))
        || (dest.contains("turbo-q8") && name.contains("turbo-q8"))
}

/// 条目接受的体积下限：显式 `min_size` 优先，否则按 `size` 的 95% 推导。
///
/// 与 `model_download` 的判定同源（那里是私有实现，这里是它的文档化复刻），
/// 目的只有一个——体检报告与「已就位」判定**不能给出互相矛盾**的结论。
fn effective_min_size(item: &DownloadItem) -> u64 {
    if item.min_size > 0 {
        item.min_size
    } else {
        (item.size as f64 * 0.95) as u64
    }
}

/// 非 Ok 项的建议行；Ok 项返回 `None`。
fn hint_for(id: &str, status: CheckStatus) -> Option<&'static str> {
    if status == CheckStatus::Ok {
        return None;
    }
    Some(match id {
        "project_root" => "确认启动目录：项目根的判据是根下存在 models/；也可用 VOICE2WORD_HOME 指定。",
        "ffmpeg" => "下载 FFmpeg 放到 tools/ffmpeg.exe，或把 config.local.toml 的 paths.ffmpeg 指到实际位置。",
        "whisper_cli" => "把缺失的 DLL 放到 exe 同目录，或换用官方 MSVC 构建（自带 VC++ 运行库依赖）。",
        "whisper_model" => "重新下载该模型：截断文件要到加载时才报错，体积对比是唯一的早期信号。",
        "llama_cpp" => "把缺失的 DLL 放到 exe 同目录；官方包解压不完整时重新解压一次。",
        "llm_model" => "换成 .gguf 模型文件，并把 config 的 paths.llm_model 指过去。",
        "python" => "若需指定解释器，在 config.local.toml 里把 paths.python 写成绝对路径。",
        "database" => "首次运行会自动创建；若已存在却报打不开，检查文件权限与磁盘。",
        "config_files" => "从仓库重新取一份 config.toml 放到项目根（config.local.toml 可选）。",
        "data_dir_writable" => "把程序（及项目根）放到当前用户可写的目录，或修复该目录的权限。",
        _ => "请把本报告整段复制反馈。",
    })
}

/// 三态计数：`(Ok, Warn, Fail)`。
fn count_statuses(items: &[CheckItem]) -> (usize, usize, usize) {
    let mut ok = 0;
    let mut warn = 0;
    let mut fail = 0;
    for item in items {
        match item.status {
            CheckStatus::Ok => ok += 1,
            CheckStatus::Warn => warn += 1,
            CheckStatus::Fail => fail += 1,
        }
    }
    (ok, warn, fail)
}

/// 在 `root` 内写一个临时文件再删掉，验证「可写」。失败时返回原因字符串。
///
/// 文件名带 pid，避免并行测试 / 两个实例互相覆盖；无论成败都尝试删除，
/// 不给用户的项目根留垃圾。
fn probe_writable(root: &Path) -> Result<(), String> {
    let probe = root.join(format!(".v2w_write_probe_{}", std::process::id()));
    let write = std::fs::write(&probe, b"voice2word-diagnostics").map_err(|e| e.to_string());
    let cleanup = std::fs::remove_file(&probe).map_err(|e| e.to_string());
    write.and(cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个专属临时目录（先清后建），并在其中放好配置。
    ///
    /// 为什么用 `pid` 做后缀：cargo 默认并行跑测试，同一测试文件里的多个用例
    /// 若共用一个目录会互相删对方的文件；带上进程 id 再各自带 tag 就完全隔离。
    /// 注意所有路径都指向临时目录，**绝不**写进仓库树。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("v2w_diag_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 把所有组件路径都指到临时目录里的（默认不存在的）文件的配置。
    fn cfg_in(dir: &Path) -> AppConfig {
        let mut cfg = AppConfig::default();
        let p = |name: &str| dir.join(name).to_string_lossy().into_owned();
        cfg.paths.ffmpeg = p("ffmpeg.exe");
        cfg.paths.whisper_cli = p("whisper-cli.exe");
        cfg.paths.whisper_model = p("ggml-small-q5_0.bin");
        cfg.paths.llama_cli = p("llama-completion.exe");
        cfg.paths.llm_model = p("qwen.gguf");
        cfg.paths.python = "python".to_string();
        cfg
    }

    /// 按 id 取检查项（找不到就 panic：测试里缺项本身就是失败）。
    fn item<'a>(items: &'a [CheckItem], id: &str) -> &'a CheckItem {
        items
            .iter()
            .find(|i| i.id == id)
            .unwrap_or_else(|| panic!("报告里缺少检查项 {id}"))
    }

    /// 全缺失的配置：报告要能生成、不能 panic，且各「必需组件」落在 Fail。
    #[test]
    fn all_missing_config_reports_fails_without_panic() {
        let dir = temp_dir("all_missing");
        let cfg = cfg_in(&dir);
        let items = run_checks(&cfg, &dir);
        for id in [
            "ffmpeg",
            "whisper_cli",
            "whisper_model",
            "llama_cpp",
            "llm_model",
        ] {
            assert_eq!(item(&items, id).status, CheckStatus::Fail, "{id} 应为 Fail");
        }
        // 数据库缺失只是 Warn（首次运行会创建），配置文件缺失才是 Fail。
        assert_eq!(item(&items, "database").status, CheckStatus::Warn);
        assert_eq!(item(&items, "config_files").status, CheckStatus::Fail);
        // 目录存在但没有 models/ → Warn；可写 → Ok。
        assert_eq!(item(&items, "project_root").status, CheckStatus::Warn);
        assert_eq!(item(&items, "data_dir_writable").status, CheckStatus::Ok);
        let report = build_report(&cfg, &dir);
        assert!(report.contains("Voice2Word 诊断报告"));
        assert!(report.contains("[FAIL] FFmpeg: "));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 临时 ffmpeg 文件存在 → `Ok`，详情里带路径与体积。
    #[test]
    fn temp_ffmpeg_file_is_ok() {
        let dir = temp_dir("ffmpeg_ok");
        let cfg = cfg_in(&dir);
        std::fs::write(&cfg.paths.ffmpeg, b"fake-ffmpeg").expect("写假 ffmpeg");
        let items = run_checks(&cfg, &dir);
        let ffmpeg = item(&items, "ffmpeg");
        assert_eq!(ffmpeg.status, CheckStatus::Ok);
        assert!(ffmpeg.detail.contains("ffmpeg.exe"), "详情应含实际路径");
        assert!(
            ffmpeg.detail.contains("11 B"),
            "详情应含体积: {}",
            ffmpeg.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 体积远小于登记下限的 whisper 模型 → `Warn`（截断下载的典型形态）。
    #[test]
    fn truncated_whisper_model_warns() {
        let dir = temp_dir("whisper_truncated");
        let cfg = cfg_in(&dir);
        std::fs::write(&cfg.paths.whisper_model, b"tiny").expect("写截断模型");
        let items = run_checks(&cfg, &dir);
        let model = item(&items, "whisper_model");
        assert_eq!(model.status, CheckStatus::Warn, "截断模型应为 Warn");
        assert!(model.detail.contains("疑似下载被截断"), "{}", model.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 `.gguf` 的 LLM 模型 → `Warn`（llama.cpp 会拒绝加载）。
    #[test]
    fn non_gguf_llm_model_warns() {
        let dir = temp_dir("llm_ext");
        let mut cfg = cfg_in(&dir);
        cfg.paths.llm_model = dir.join("model.bin").to_string_lossy().into_owned();
        std::fs::write(&cfg.paths.llm_model, b"not-a-gguf").expect("写模型");
        let items = run_checks(&cfg, &dir);
        let llm = item(&items, "llm_model");
        assert_eq!(llm.status, CheckStatus::Warn);
        assert!(llm.detail.contains(".gguf"), "{}", llm.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 数据库：缺失 → `Warn`；存在 → `Ok` 且说明 `-wal` 伴随文件。
    #[test]
    fn database_missing_warns_present_ok() {
        let dir = temp_dir("db");
        let cfg = cfg_in(&dir);
        let items = run_checks(&cfg, &dir);
        assert_eq!(item(&items, "database").status, CheckStatus::Warn);

        let db = dir.join("voice2word.db");
        std::fs::write(&db, b"sqlite").expect("写库文件");
        let items = run_checks(&cfg, &dir);
        let present = item(&items, "database");
        assert_eq!(present.status, CheckStatus::Ok);
        assert!(
            present.detail.contains("无 -wal 伴随文件"),
            "{}",
            present.detail
        );

        std::fs::write(dir.join("voice2word.db-wal"), b"wal").expect("写 wal");
        let items = run_checks(&cfg, &dir);
        let with_wal = item(&items, "database");
        assert!(
            with_wal.detail.contains("WAL 模式下正常"),
            "{}",
            with_wal.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 临时目录可写 → `data_dir_writable` 为 `Ok`，且探针文件用完即删。
    #[test]
    fn data_dir_writable_ok_in_temp_dir() {
        let dir = temp_dir("writable");
        let cfg = cfg_in(&dir);
        let items = run_checks(&cfg, &dir);
        assert_eq!(item(&items, "data_dir_writable").status, CheckStatus::Ok);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("读目录")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".v2w_write_probe_")
            })
            .collect();
        assert!(leftovers.is_empty(), "探针文件不应残留: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// id 唯一且顺序与文档一致（报告顺序就是用户阅读顺序，不能随手改）。
    #[test]
    fn check_ids_are_unique_and_ordered() {
        let dir = temp_dir("ids");
        let cfg = cfg_in(&dir);
        let items = run_checks(&cfg, &dir);
        let ids: Vec<&str> = items.iter().map(|i| i.id).collect();
        assert_eq!(
            ids,
            vec![
                "project_root",
                "ffmpeg",
                "whisper_cli",
                "whisper_model",
                "llama_cpp",
                "llm_model",
                "python",
                "database",
                "config_files",
                "data_dir_writable",
            ]
        );
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "检查项 id 必须唯一");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 报告必须逐项包含每个检查项的标签与详情（否则用户根本看不到那一条）。
    #[test]
    fn report_contains_every_item_label() {
        let dir = temp_dir("labels");
        let cfg = cfg_in(&dir);
        let items = run_checks(&cfg, &dir);
        let report = build_report(&cfg, &dir);
        for entry in &items {
            assert!(
                report.contains(&entry.label),
                "报告缺少标签 {}",
                entry.label
            );
            assert!(
                report.contains(&entry.detail),
                "报告缺少详情 {}",
                entry.detail
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 报告刻意不含密钥：塞进配置的假密钥绝不能出现在输出里。
    #[test]
    fn report_omits_api_key() {
        let dir = temp_dir("secrets");
        let mut cfg = cfg_in(&dir);
        const PLANTED: &str = "sk-planted-diagnostic-key-do-not-leak";
        cfg.translate.api_key = PLANTED.to_string();
        let report = build_report(&cfg, &dir);
        assert!(!report.contains(PLANTED), "报告泄露了 API Key");
        assert!(!report.contains("sk-"), "报告出现了疑似密钥片段");
        assert!(report.contains("不包含任何密钥"), "应明写不含密钥");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 确定性：同样输入两次调用必须逐字节相同（否则测试无法断言、用户无法 diff）。
    #[test]
    fn report_is_deterministic() {
        let dir = temp_dir("deterministic");
        let cfg = cfg_in(&dir);
        std::fs::write(&cfg.paths.ffmpeg, b"x").expect("写文件");
        let first = build_report(&cfg, &dir);
        let second = build_report(&cfg, &dir);
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 项目根根本不存在时也不能 panic：全部退化成 Fail / Warn，报告照样生成。
    #[test]
    fn nonexistent_root_does_not_panic() {
        let dir = temp_dir("no_root");
        let missing = dir.join("does-not-exist");
        let cfg = cfg_in(&missing);
        let items = run_checks(&cfg, &missing);
        assert_eq!(items.len(), 10, "缺项也要给满十条");
        assert_eq!(item(&items, "project_root").status, CheckStatus::Fail);
        assert_eq!(item(&items, "data_dir_writable").status, CheckStatus::Fail);
        assert_eq!(item(&items, "ffmpeg").status, CheckStatus::Fail);
        let report = build_report(&cfg, &missing);
        assert!(report.contains("[FAIL] 项目根: "));
        assert!(report.contains("汇总："));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
