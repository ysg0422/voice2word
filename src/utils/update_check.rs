//! 更新检查：判断「有没有新版」，并把安装包下载到 `updates/` 目录（不负责安装）。
//!
//! # 为什么需要它
//!
//! 本程序以单个 exe 发布，用户拿到的是一份拷贝。上游发了新版本之后，用户没有任何
//! 渠道能知道——除非自己回仓库页面看。结果是用户长期停在旧版本上：已经修好的 bug
//! 还在反馈，已经填掉的坑还在踩。本模块补上「告知」这一环：向 GitHub Releases API
//! 要最新 tag，和编译进二进制的版本比较，**只有更新时**才把版本号、发行说明
//! （截断后的）、下载页与附件直链交出去。
//!
//! # 它**不做**什么
//!
//! 不**安装**：`download_update_file` 只把附件落到 `updates/` 目录（先写 `*.part`
//! 再 `rename`），绝不静默替换正在运行的 exe——文件被占用导致替换失败、替换到一半
//! 断电留下半个 exe、被杀软或签名校验拦下……这些失败的代价远大于「用户自己多点
//! 一次下载页」。替换/安装由用户自己完成。
//!
//! # 落盘前的校验（为什么不止「大小 + 非空」）
//!
//! 下载走的是第三方加速镜像：镜像返回什么字节完全不受 GitHub 控制。只查长度与
//! 「开头是不是 MZ」挡不住换包——一个带 MZ 头的陌生 exe 会被当成「新版本安装包」
//! 落到 `updates/`，再引导用户去运行。因此除了长度与魔数，还会校验**官方 API 声明
//! 的 sha256**（附件元数据的 `digest` 字段）：见 [`fetch_latest_release`] 的登记与
//! [`download_update_file`] 的比对。上游没给摘要时退化为长度 + 魔数，并在日志里
//! 明确记下「本次无从校验」。
//!
//! # 网络与调用时机
//!
//! [`fetch_latest_release`] 会发起**一次外网请求**
//! （`GET https://api.github.com/repos/.../releases/latest`）。它只在用户**主动**
//! 点「检查更新」时被调用；界面不会后台轮询，程序启动也不会自动联网——本程序主打
//! 「完全本地运行」，偷偷联网会破坏这个承诺，还会白白撞上 GitHub 的按 IP 限流。
//!
//! [`download_update_file`] 只在用户点了「下载」之后才发起请求，并且和
//! [`super::model_download`] 里的模型下载一样走加速镜像：GitHub 直链先试
//! `ghproxy.net` / `gh-proxy.com` / `mirror.ghproxy.com`，全部失败才回退直连。
//! 失败时给出可操作的中文错误（见 [`fetch_latest_release`]），由界面决定怎么展示。
//!
//! # 结构
//!
//! 解析（[`parse_latest_release`]）、版本比较（[`is_newer`]）、文案组装
//! （[`describe`]）与落盘前的校验（`verify_download` / `magic_matches`）都是纯函数，
//! 因此能被完整单测覆盖；碰 IO 的只有 [`fetch_latest_release`] 与
//! [`download_update_file`]。

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use tracing::warn;

/// GitHub「最新发行版」接口地址。
///
/// `owner/repo` 直接写死：二进制没法在运行时知道自己属于哪个仓库的 release
/// （仓库地址见 README：`https://github.com/ysg0422/voice2word`）。
/// 单独提成常量，一是能被 grep 到，二是能被测试断言形状。
pub const RELEASES_API_URL: &str =
    "https://api.github.com/repos/ysg0422/voice2word/releases/latest";

/// 请求 GitHub 时用的 `User-Agent`。
///
/// **GitHub API 强制要求**带一个非空 `User-Agent`：不带会直接返回 403，
/// 而 403 的错误体是限流说明，看起来像是「被限流」，排查方向会被带偏。
const USER_AGENT: &str = concat!("voice2word/", env!("CARGO_PKG_VERSION"));

/// 检查更新的超时下限（秒）：再短在慢网上几乎必然失败。
const MIN_TIMEOUT_SECS: u64 = 3;

/// 检查更新的超时上限（秒）：再长会让「检查更新」按钮看起来像卡死了。
const MAX_TIMEOUT_SECS: u64 = 60;

/// 发行说明最多保留的字符数。
pub const MAX_NOTES_CHARS: usize = 1200;

/// 一次更新检查的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRelease {
    /// 去掉 leading `v` 的版本号，例如 `0.2.1`
    pub version: String,
    /// 发布日期（`published_at` 原文；缺失为空串）
    pub published_at: String,
    /// 下载页（`html_url`）
    pub page_url: String,
    /// 发行说明（已截断，见 `MAX_NOTES_CHARS`）
    pub notes: String,
    /// 直接下载链接（提取 .exe 或 .zip 资产，无需去网页点击）
    pub download_url: String,
    /// 附件名称（如 voice2word.exe）
    pub asset_name: String,
    /// 附件大小（字节，0 表示未知）
    pub asset_size: u64,
    /// 附件内容的 sha256（取自 GitHub 附件元数据的 `digest` 字段，小写十六进制）。
    ///
    /// 这个值是用来**校验下载内容**的。下载走的是第三方加速镜像，镜像返回什么字节
    /// 完全不受 GitHub 控制；只查「长度够不够、开头是不是 MZ」挡不住换包——一个
    /// 带 MZ 头的陌生 exe 会被当成「新版本安装包」落到 `updates/`，再引导用户去运行。
    /// 官方 API 直接给出的摘要把「镜像返回的字节」与「官方发布的字节」绑在一起，
    /// 镜像中途换内容就会在落盘前被发现。
    ///
    /// 上游没提供 `digest` 时为 `None`：那种情况下无从校验，仍走长度 + 魔数。
    pub asset_sha256: Option<String>,
}

/// 当前编译进二进制的版本（`CARGO_PKG_VERSION`）。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 解析 GitHub `/releases/latest` 的响应体。
///
/// 除 `tag_name` 外的字段缺失都当作空串（GitHub 在不同情况下会省略
/// `published_at` / `body`）；`tag_name` 缺失或不是字符串则返回 `Err`，
/// 因为**没有版本号就什么都判断不了**，硬凑一个空版本只会污染界面。
///
/// `notes` 按**字符**截断到 [`MAX_NOTES_CHARS`]（`chars().take()`）。
/// 不能用字节切片：发行说明里常有中文或 emoji，按字节切会把一个多字节字符劈开，
/// 结果是 `panic`（`byte index is not a char boundary`）——这是处理 CJK 文本
/// 最经典的坑，而 release notes 恰恰极可能整段是中文。
pub fn parse_latest_release(body: &str) -> Result<LatestRelease> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| anyhow!("发行版信息不是合法 JSON：{e}"))?;

    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("发行版信息缺少 tag_name，无法判断版本号"))?;

    let version = tag.strip_prefix(['v', 'V']).unwrap_or(tag).to_string();

    let text_field = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };

    // 按字符截断：CJK 发行说明按字节切会 panic，见函数文档。
    let notes: String = value
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .chars()
        .take(MAX_NOTES_CHARS)
        .collect();

    // 从 assets 列表中提取可执行文件或安装包附件直链：
    // 优先级策略：
    // 1. *-Setup.exe（向导安装包，最推荐，能自动升级现有安装并配置好快捷方式）
    // 2. voice2word.exe（主程序便携版，单文件直接运行）
    // 3. 其他非 cli 的 .exe
    // 4. .zip 压缩包
    // 5. voice2word-cli.exe（纯无头后台命令行工具，仅在别无选择时作为最后备选）
    let asset_rank = |name: &str| -> i32 {
        let lower = name.to_ascii_lowercase();
        if lower.contains("setup") && lower.ends_with(".exe") {
            100
        } else if (lower == "voice2word.exe" || lower.starts_with("voice2word-"))
            && lower.ends_with(".exe")
            && !lower.contains("cli")
        {
            80
        } else if lower.ends_with(".exe") && !lower.contains("cli") {
            50
        } else if lower.ends_with(".zip") {
            30
        } else if lower.ends_with(".exe") {
            10
        } else {
            0
        }
    };

    let mut download_url = String::new();
    let mut asset_name = String::new();
    let mut asset_size: u64 = 0;
    let mut asset_sha256: Option<String> = None;
    let mut best_rank = 0;

    if let Some(assets) = value.get("assets").and_then(|a| a.as_array()) {
        for asset in assets {
            if let Some(name) = asset.get("name").and_then(|n| n.as_str()) {
                let rank = asset_rank(name);
                if rank == 0 {
                    continue;
                }
                let url = asset
                    .get("browser_download_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if url.is_empty() {
                    continue;
                }
                let size = asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                let digest = asset
                    .get("digest")
                    .and_then(|d| d.as_str())
                    .and_then(normalize_digest);

                if rank > best_rank {
                    best_rank = rank;
                    download_url = url;
                    asset_name = name.to_string();
                    asset_size = size;
                    asset_sha256 = digest;
                }
            }
        }
    }

    if download_url.is_empty() {
        asset_name = "voice2word.exe".to_string();
        download_url = format!(
            "https://github.com/ysg0422/voice2word/releases/download/{}/voice2word.exe",
            tag
        );
    }

    Ok(LatestRelease {
        version,
        published_at: text_field("published_at"),
        page_url: text_field("html_url"),
        notes,
        download_url,
        asset_name,
        asset_size,
        asset_sha256,
    })
}

/// 归一化附件元数据里的 `digest` 字段：只接受 `sha256:<64 位十六进制>`。
///
/// 其它算法（`md5:` / `sha1:`）与形状不对的串一律返回 `None`——宁可不校验，也不能
/// 拿一个没打算支持的算法假装「校验过了」。返回值统一成小写十六进制，便于与算出来的
/// 摘要直接比较。
fn normalize_digest(raw: &str) -> Option<String> {
    let (algo, hex) = raw.trim().split_once(':')?;
    if !algo.eq_ignore_ascii_case("sha256") {
        return None;
    }
    let hex = hex.trim();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

/// 摘要比对：十六进制大小写两种写法都合法，因此大小写不敏感。
fn digests_match(expected: &str, actual: &str) -> bool {
    expected.eq_ignore_ascii_case(actual)
}

/// 版本号里的一段：整数部分 + 可选的预发布后缀。
#[derive(Debug, Clone, PartialEq, Eq)]
struct VersionSegment {
    /// 整数部分（`0-beta` 的 `0`；纯非数字段如 `beta` 记为 0）
    number: u64,
    /// 预发布后缀（`0-beta` 里的 `beta`）；正式版段为 `None`
    prerelease: Option<String>,
}

/// 解析十进制整数：空串当 0，超长数字饱和到 `u64::MAX`，**不 panic**。
///
/// 为什么要饱和而不是报错：`0.99999999999999999999` 这类超长段在
/// `parse::<u64>()` 会返回 `Err`。`unwrap` 会 panic；把整段判成「无法解析」
/// 又会让「比当前版本新」这个显而易见的事实变成 `false`（规则 5 只该拦住真正
/// 看不懂的版本号，不该拦住数字写得过长的）。饱和到上界既保住不 panic，
/// 也让超大段稳定地比任何正常版本号大。
fn parse_u64_saturating(digits: &str) -> u64 {
    if digits.is_empty() {
        return 0;
    }
    digits.parse::<u64>().unwrap_or(u64::MAX)
}

/// 解析单段：切出前导数字与其余部分，其余部分非空即视为预发布后缀。
fn parse_segment(part: &str) -> Option<VersionSegment> {
    if part.is_empty() {
        return None;
    }
    // 前导数字之后的下标一定落在字符边界上（ASCII 数字只占 1 字节）。
    let digits_end = part
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(part.len());
    let (digits, rest) = part.split_at(digits_end);
    Some(VersionSegment {
        number: parse_u64_saturating(digits),
        prerelease: if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        },
    })
}

/// 把版本串解析成段列表；空串或含空段等一律返回 `None`。
fn parse_version(text: &str) -> Option<Vec<VersionSegment>> {
    let trimmed = text.trim();
    let without_v = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    if without_v.is_empty() {
        return None;
    }
    // 规则 5 的「全非数字」：整个串一个数字都没有（例如 `abc`）就没有任何可比较的
    // 信息，直接判为无法解析。注意这与规则 4 不冲突——规则 4 说的是**单个段**
    // （`0-beta` 这种带前导数字的）里的非数字后缀。
    if !without_v.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut segments = Vec::new();
    for part in without_v.split('.') {
        segments.push(parse_segment(part)?);
    }
    Some(segments)
}

/// 语义化版本比较：`a` 是否比 `b` 新。
///
/// 规则（按此顺序，**必须每条都有测试**）：
/// 1. 先剥掉可选的 leading `v` / `V`；
/// 2. 按 `.` 切段，逐段按**整数**比较（`0.10.0` 比 `0.9.0` 新 —— 字符串比较会判反，
///    这是版本比较最经典的坑）；
/// 3. 段数不等时缺失段视为 0（`0.2` == `0.2.0`，`0.2.1` > `0.2`）；
/// 4. 任何含预发布后缀的段（例如 `1.0.0-beta.1` 的 `0-beta`）在**数值相等时**
///    视为更旧（`1.0.0` > `1.0.0-beta`），这是 semver 的约定；含非数字且非空的
///    段一律当预发布处理；
/// 5. 完全无法解析（空串、全非数字）→ 返回 `false`（宁可说「不是新版」，
///    也不要提示用户去升级一个我们看不懂的版本号）。
///
/// 两个都是预发布、且整数部分相等时，按后缀的字典序比较（`beta` > `alpha`），
/// 足以满足 `alpha < beta < rc` 这类常见命名。
pub fn is_newer(a: &str, b: &str) -> bool {
    let (Some(mut va), Some(mut vb)) = (parse_version(a), parse_version(b)) else {
        return false;
    };

    // 缺失段补 0（规则 3），这样后面可以按位对齐比较。
    let len = va.len().max(vb.len());
    let zero = VersionSegment {
        number: 0,
        prerelease: None,
    };
    va.resize(len, zero.clone());
    vb.resize(len, zero);

    for (sa, sb) in va.iter().zip(vb.iter()) {
        if sa.number != sb.number {
            return sa.number > sb.number;
        }
        match (&sa.prerelease, &sb.prerelease) {
            (None, None) => {}
            // 数值相等时带预发布后缀的一侧更旧（规则 4）。
            (Some(_), None) => return false,
            (None, Some(_)) => return true,
            (Some(pa), Some(pb)) => {
                if pa != pb {
                    return pa > pb;
                }
            }
        }
    }
    false
}

/// 请求 GitHub 最新发行版。网络失败/无发行版/解析失败都要给出可操作的中文错误。
///
/// `timeout_secs` 会被夹到 [`MIN_TIMEOUT_SECS`]..=[`MAX_TIMEOUT_SECS`]：
/// 调用方传 0 或传一个夸张的大数都不该把这次检查变成「立刻失败」或「永远转圈」。
///
/// 用 `agent.get(url).call()` 并**逐一映射错误**（与 `engines/translate.rs` 的
/// 在线接口调用同风格）：任何分支都返回 `Err`，绝不 panic。GitHub 要求请求头里
/// 带 `User-Agent`（见 [`USER_AGENT`]），并推荐
/// `Accept: application/vnd.github+json`。
pub fn fetch_latest_release(timeout_secs: u64) -> Result<LatestRelease> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(
            timeout_secs.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS),
        ))
        .try_proxy_from_env(true)
        .build();

    let response = match agent
        .get(RELEASES_API_URL)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => return Err(http_status_error(code, resp)),
        Err(e) => {
            return Err(anyhow!(
                "检查更新失败：连接 GitHub 失败（{e}）。请确认网络可用后重试"
            ));
        }
    };

    // ureq 只把 >=400 当错误；这里再兜一层，保证「非 2xx 一律给状态码」。
    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(http_status_error(status, response));
    }

    let body = response
        .into_string()
        .map_err(|e| anyhow!("检查更新失败：读取 GitHub 响应出错（{e}）"))?;
    let release = parse_latest_release(&body)?;
    // 把官方声明的内容摘要登记下来，供随后的下载校验使用（见 [`ADVERTISED_DIGESTS`]）。
    record_advertised_digest(&release.download_url, release.asset_sha256.as_deref());
    if release.asset_sha256.is_none() {
        warn!(
            asset = %release.asset_name,
            "上游未提供附件内容摘要，本次下载只能按长度与文件头校验"
        );
    }
    Ok(release)
}

/// 把非 2xx 响应变成带状态码与提示的中文错误。
///
/// 403 / 404 是两种「不是网络问题、但用户看不懂」的典型情况：
/// - 403：GitHub API 按 IP 限流（匿名额度很小），提示稍后再试；
/// - 404：仓库根本还没发布过 Release。
///
/// 不带上这两句提示，用户只会看到一坨英文 JSON，不知道该干什么。
fn http_status_error(code: u16, resp: ureq::Response) -> anyhow::Error {
    let hint = match code {
        403 => "（GitHub API 限流，请稍后再试）",
        404 => "（该仓库还没有发布任何 Release）",
        _ => "",
    };
    // 错误体可能很长，截一小段给用户看即可（按字符截，避免劈开 CJK）。
    let body = resp.into_string().unwrap_or_default();
    let snippet: String = body.chars().take(200).collect();
    anyhow!("检查更新失败：GitHub 返回 HTTP {code}{hint}：{snippet}")
}

/// 组装给界面显示的一句话（纯函数）。`newer` 为真时形如
/// `发现新版本 0.2.1（当前 0.1.0）`，否则 `已是最新版本 0.1.0`。
pub fn describe(current: &str, latest: &str, newer: bool) -> String {
    if newer {
        format!("发现新版本 {latest}（当前 {current}）")
    } else {
        format!("已是最新版本 {current}")
    }
}

/// 判断一次下载是否算「完整」。
///
/// 服务器给了 `Content-Length` 时，收到的字节数不能少于它：镜像/代理中途断流
/// 时 `read` 会以 `Ok(0)` 正常收尾，若只看「有没有读到过数据」，一个残缺的 exe
/// 会被当成成功落盘，用户点更新后拿到的是打不开的文件。未给长度（chunked 传输）
/// 时无从校验，只能放行。
fn download_is_complete(written: u64, total_size: u64) -> bool {
    total_size == 0 || written >= total_size
}

/// 下载内容的最小可信字节数：比它小的一律当成下载源返回的错误页 / 空响应丢弃。
///
/// 真实的 Windows 安装包是几十 MB，1 KB 这个下限只用来挡住「HTTP 200 + 一小段
/// 正文」这类一眼就不是附件的响应。
const MIN_DOWNLOAD_BYTES: u64 = 1024;

/// 落盘前的**长度**校验：`Ok(())` 表示这份内容可以落盘，`Err(原因)` 表示该下载源
/// 不可用（调用方换下一个镜像，并把原因留到全部失败时报给用户）。
///
/// 抽成纯函数是为了能被单测覆盖：`download_update_file` 本体要联网，测不到。
/// 两种失败必须都给出原因——只删掉 `.part` 而不记 `last_err`，最后会报成
/// 「所有下载镜像均不可用」，与真实原因（内容太小 / 被丢弃）完全对不上。
fn verify_download(written: u64, total_size: u64) -> Result<(), String> {
    if !download_is_complete(written, total_size) {
        return Err(format!(
            "下载不完整：收到 {written} 字节，Content-Length 声明 {total_size} 字节"
        ));
    }
    if written <= MIN_DOWNLOAD_BYTES {
        return Err(format!(
            "下载内容只有 {written} 字节，疑似下载源返回了错误页而不是安装包"
        ));
    }
    Ok(())
}

/// 把服务端给的附件名收敛成一个**纯文件名**。
///
/// 附件名来自 GitHub API，`PathBuf::join` 遇到绝对路径会整体替换目标、遇到 `..`
/// 会向上逃逸：`..\\..\\evil.exe` 会把文件写到 `updates/` 目录之外。这里只保留最后
/// 一个路径分隔符之后的部分，并挡住 `.` / `..` / 空串。
fn safe_file_name(raw: &str) -> String {
    let base = raw
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    if base.is_empty() || base == "." || base == ".." {
        return "voice2word.exe".to_string();
    }
    base.to_string()
}

/// 附件应有的文件头（魔数）；未知扩展名返回 `None`（不校验，保持既有行为）。
fn expected_magic(name: &str) -> Option<&'static [u8]> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".exe") {
        Some(b"MZ")
    } else if lower.ends_with(".zip") {
        Some(b"PK\x03\x04")
    } else {
        None
    }
}

/// 已收到的内容开头是否与附件类型相符。
///
/// 镜像 / 代理在限流或出错时经常返回 `HTTP 200` + 一小段 HTML，而且
/// `Content-Length` 与这段 HTML 的长度一致，长度校验完全看不出问题；只看长度就会
/// 把错误页 `rename` 成 `voice2word.exe`，界面还提示「新版本已下载就绪」。
/// 因此落盘前再按魔数确认一次。
fn magic_matches(name: &str, head: &[u8]) -> bool {
    match expected_magic(name) {
        None => true,
        Some(magic) => head.starts_with(magic),
    }
}

// 官方 API 声明过内容摘要的下载链接 → 摘要。
//
// 为什么需要这个全局表：界面的「检查更新 → 下载」两步之间只传 `(URL, 文件名, 大小)`，
// 摘要没有随参数传进来（要改这条链上的类型就得动界面层）。而官方 API 声明的摘要
// 正是校验「第三方镜像返回的字节」的唯一可信依据，所以由 `fetch_latest_release`
// 在拿到响应时登记，`download_update_file` 按 URL 取回。
//
// 只保留最近 MAX_ADVERTISED_DIGESTS 条：这是会话级缓存，不是历史记录。
static ADVERTISED_DIGESTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// 最多记住多少条已声明的摘要。
const MAX_ADVERTISED_DIGESTS: usize = 8;

/// 登记一条「官方 API 为这个下载链接声明的内容摘要」；`sha256` 为 `None` 时不登记。
fn record_advertised_digest(url: &str, sha256: Option<&str>) {
    let Some(digest) = sha256 else {
        return;
    };
    if url.trim().is_empty() {
        return;
    }
    // 中毒只意味着这一次没登记上，绝不能因此打断「检查更新」本身。
    let Ok(mut list) = ADVERTISED_DIGESTS.lock() else {
        return;
    };
    list.retain(|(u, _)| u != url);
    list.insert(0, (url.to_string(), digest.to_string()));
    list.truncate(MAX_ADVERTISED_DIGESTS);
}

/// 取回某个下载链接被官方声明过的内容摘要（没有则 `None`，即「无从校验」）。
fn advertised_digest(url: &str) -> Option<String> {
    let list = ADVERTISED_DIGESTS.lock().ok()?;
    list.iter().find(|(u, _)| u == url).map(|(_, d)| d.clone())
}

/// 边写盘边算 sha256 —— 校验不必为了拿摘要再把几十 MB 的文件整份读一遍。
///
/// `new(false)` 时所有 `update` 都是空操作、`finish_hex` 返回 `None`：上游没给摘要
/// 时不做无谓的哈希计算。
struct StreamingHasher(Option<Sha256>);

impl StreamingHasher {
    fn new(enabled: bool) -> Self {
        Self(enabled.then(Sha256::new))
    }

    fn update(&mut self, buf: &[u8]) {
        if let Some(hasher) = self.0.as_mut() {
            hasher.update(buf);
        }
    }

    fn finish_hex(self) -> Option<String> {
        self.0.map(|hasher| format!("{:x}", hasher.finalize()))
    }
}

/// 下载更新文件到 updates/ 目录，支持镜像加速与断点/临时文件保护。
pub fn download_update_file(
    download_url: &str,
    asset_name: &str,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    progress: impl Fn(f32, u64, u64) + Send + 'static,
) -> Result<std::path::PathBuf> {
    use std::fs;
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;

    let updates_dir = crate::utils::AppConfig::resolve_path("updates");
    fs::create_dir_all(&updates_dir).map_err(|e| anyhow!("创建 updates 目录失败：{e}"))?;

    let final_name = safe_file_name(asset_name);
    let dest = updates_dir.join(&final_name);
    let part = updates_dir.join(format!("{final_name}.part"));
    // 本次下载要校验的内容摘要：只有官方 API 为**这个** URL 声明过才有值。
    // 有值时「镜像返回的字节」必须与官方发布的一致，否则换下一个源。
    let expected_digest = advertised_digest(download_url);

    // 候选下载镜像（国内加速镜像优先，最后兜底 GitHub 直链）
    // 候选下载镜像（国内加速镜像优先，最后兜底 GitHub 直链）
    let mut candidate_urls = Vec::new();
    if download_url.starts_with("https://github.com/") {
        candidate_urls.push(format!("https://ghfast.top/{download_url}"));
        candidate_urls.push(format!("https://gh-proxy.com/{download_url}"));
        candidate_urls.push(format!("https://mirror.ghproxy.com/{download_url}"));
        candidate_urls.push(format!("https://ghproxy.net/{download_url}"));
    }
    candidate_urls.push(download_url.to_string());

    let mut last_err = None;
    for url in candidate_urls {
        if cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&part);
            return Err(anyhow!("用户已取消下载"));
        }

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(6))
            .timeout_read(Duration::from_secs(60))
            .try_proxy_from_env(true)
            .build();

        let resp = match agent
            .get(&url)
            .set(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            )
            .call()
        {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow!("连接下载源失败：{e}"));
                continue;
            }
        };

        if resp.status() < 200 || resp.status() >= 300 {
            last_err = Some(anyhow!("下载源返回 HTTP {}", resp.status()));
            continue;
        }

        let total_size = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);

        let mut reader = resp.into_reader();
        let mut file = match fs::File::create(&part) {
            Ok(f) => f,
            Err(e) => return Err(anyhow!("创建临时文件失败：{e}")),
        };

        let mut buf = vec![0u8; 128 * 1024];
        let mut written: u64 = 0;
        let mut download_ok = true;
        // 落盘前要做魔数校验，但那时文件已经写到磁盘上了；顺手在下载过程中记住开头
        // 几个字节，省得再把文件打开一次。
        let mut head = [0u8; 4];
        let mut head_len = 0usize;
        // 摘要随写盘一起算：下完再整份读一遍是另一笔几十 MB 的 IO。
        let mut hasher = StreamingHasher::new(expected_digest.is_some());

        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = fs::remove_file(&part);
                return Err(anyhow!("用户已取消下载"));
            }

            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if head_len < head.len() {
                        let take = (head.len() - head_len).min(n);
                        head[head_len..head_len + take].copy_from_slice(&buf[..take]);
                        head_len += take;
                    }
                    if let Err(e) = file.write_all(&buf[..n]) {
                        last_err = Some(anyhow!("写入磁盘失败：{e}"));
                        download_ok = false;
                        break;
                    }
                    written += n as u64;
                    hasher.update(&buf[..n]);
                    let frac = if total_size > 0 {
                        (written as f32 / total_size as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    progress(frac, written, total_size);
                }
                Err(e) => {
                    last_err = Some(anyhow!("网络读取中断：{e}"));
                    download_ok = false;
                    break;
                }
            }
        }

        drop(file);

        // 落盘前校验：长度（截断 / 过小，见 `verify_download`）与内容类型
        // （魔数，见 `magic_matches`）。任何一种不过都当成该镜像不可用，换下一个。
        if download_ok {
            if let Err(reason) = verify_download(written, total_size) {
                last_err = Some(anyhow!("{}", reason));
                download_ok = false;
            }
        }
        if download_ok && !magic_matches(&final_name, &head[..head_len]) {
            last_err = Some(anyhow!(
                "下载内容不是有效的安装包（{final_name} 的文件头不匹配），疑似下载源返回了错误页"
            ));
            download_ok = false;
        }
        // 内容摘要校验：只有官方 API 给过摘要时才做（没摘要 = 无从校验，不能因此
        // 把文件判成坏的）。不匹配说明**镜像返回的字节不是官方发布的那一份**——
        // 这正是「换包」的形态，必须换下一个下载源，绝不能让陌生内容落盘。
        if download_ok {
            if let (Some(expected), Some(actual)) =
                (expected_digest.as_deref(), hasher.finish_hex())
            {
                if !digests_match(expected, &actual) {
                    last_err = Some(anyhow!(
                        "下载内容与官方声明的 sha256 不一致（疑似下载源替换了安装包）：\
                         期望 {expected}，实际 {actual}"
                    ));
                    download_ok = false;
                }
            }
        }

        if download_ok {
            if let Err(e) = fs::rename(&part, &dest) {
                let timestamp = chrono::Local::now().format("%Y%m%d%H%M%S");
                let alt_dest = updates_dir.join(format!("voice2word_{timestamp}.exe"));
                if let Err(e2) = fs::rename(&part, &alt_dest) {
                    return Err(anyhow!("落盘失败：{e} / {e2}"));
                }
                return Ok(alt_dest);
            }
            return Ok(dest);
        } else {
            let _ = fs::remove_file(&part);
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("所有下载镜像均不可用，请稍后重试")))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 基本大小关系：`0.2.0` 比 `0.1.0` 新，反向与相等都不成立。
    #[test]
    fn is_newer_compares_major_minor() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    /// `0.10.0` 比 `0.9.0` 新：字符串比较会得出相反结论，这是版本比较最经典的坑。
    #[test]
    fn is_newer_compares_segments_as_integers() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
    }

    /// leading `v` / `V` 必须被剥掉，否则 `v0.2.0` 会整段解析失败而永远判不出更新。
    #[test]
    fn is_newer_ignores_leading_v() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("V0.2.0", "v0.1.0"));
        assert!(!is_newer("0.1.0", "v0.2.0"));
    }

    /// 段数不等时缺失段视为 0：`0.2` == `0.2.0`，而 `0.2.1` > `0.2`。
    #[test]
    fn is_newer_treats_missing_segments_as_zero() {
        assert!(!is_newer("0.2", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2"));
        assert!(is_newer("0.2.1", "0.2"));
    }

    /// 正式版比同号预发布新（semver 约定）：`1.0.0` > `1.0.0-beta`。
    #[test]
    fn is_newer_prefers_release_over_prerelease() {
        assert!(is_newer("1.0.0", "1.0.0-beta"));
        assert!(!is_newer("1.0.0-beta", "1.0.0"));
    }

    /// 同为预发布时按后缀字典序：`beta` > `alpha`。
    #[test]
    fn is_newer_orders_prerelease_suffixes() {
        assert!(is_newer("1.0.0-beta", "1.0.0-alpha"));
        assert!(!is_newer("1.0.0-alpha", "1.0.0-beta"));
    }

    /// 无法解析的输入一律返回 `false`（宁可说「不是新版」，也别让用户去升级
    /// 一个我们看不懂的版本号）。
    #[test]
    fn is_newer_rejects_unparsable_input() {
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("0.1.0", ""));
        assert!(!is_newer("abc", "0.1.0"));
        assert!(!is_newer("0.1.0", "abc"));
        assert!(!is_newer("abc", "def"));
    }

    /// 超长数字段不能溢出 panic，且仍应被判为更新（饱和到 `u64::MAX`）。
    #[test]
    fn is_newer_survives_huge_segments() {
        assert!(is_newer("0.9999999999", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.9999999999"));
        // 比 u64 还长的数字也不能 panic
        assert!(is_newer("0.99999999999999999999999999", "0.1.0"));
    }

    /// 完整 payload：各字段逐一取出，且 `v` 前缀被剥掉。
    #[test]
    fn parse_latest_release_reads_all_fields() {
        let body = r#"{
            "tag_name": "v0.2.1",
            "published_at": "2026-01-02T03:04:05Z",
            "html_url": "https://github.com/ysg0422/voice2word/releases/tag/v0.2.1",
            "body": "本次更新：修好了长音频分块。"
        }"#;
        let r = parse_latest_release(body).expect("完整 payload 必须解析成功");
        assert_eq!(r.version, "0.2.1");
        assert_eq!(r.published_at, "2026-01-02T03:04:05Z");
        assert_eq!(
            r.page_url,
            "https://github.com/ysg0422/voice2word/releases/tag/v0.2.1"
        );
        assert_eq!(r.notes, "本次更新：修好了长音频分块。");
    }

    /// 可选字段缺失时当作空串，但 `tag_name` 仍在就返回 `Ok`。
    #[test]
    fn parse_latest_release_tolerates_missing_optional_fields() {
        let body = r#"{ "tag_name": "0.3.0" }"#;
        let r = parse_latest_release(body).expect("只有 tag_name 也应成功");
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.published_at, "");
        assert_eq!(r.page_url, "");
        assert_eq!(r.notes, "");
    }

    /// 缺 `tag_name` → `Err`（没有版本号就什么都判断不了）。
    #[test]
    fn parse_latest_release_requires_tag_name() {
        let body = r#"{ "html_url": "https://example.com" }"#;
        let err = parse_latest_release(body).expect_err("缺 tag_name 必须报错");
        assert!(
            err.to_string().contains("tag_name"),
            "错误信息要指出缺的是 tag_name，实际：{err}"
        );
    }

    /// 非 JSON 响应 → `Err`，而不是 panic。
    #[test]
    fn parse_latest_release_rejects_non_json() {
        assert!(parse_latest_release("not json at all").is_err());
    }

    /// CJK 发行说明必须按**字符**截断到上限，且不能把多字节字符劈开
    /// （按字节切会 panic，或留下 U+FFFD 替换符）。
    #[test]
    fn parse_latest_release_truncates_cjk_notes_on_char_boundary() {
        // 每个字符都是 3 字节的汉字，长度远超上限。
        let long_cjk = "更新".repeat(MAX_NOTES_CHARS); // 2 * MAX_NOTES_CHARS 个字符
        let body = format!(r#"{{ "tag_name": "v1.0.0", "body": "{long_cjk}" }}"#);
        let r = parse_latest_release(&body).expect("CJK 说明必须解析成功");
        assert_eq!(
            r.notes.chars().count(),
            MAX_NOTES_CHARS,
            "必须正好截到字符上限"
        );
        assert!(
            !r.notes.contains('\u{FFFD}'),
            "截断不能产生替换符（说明切在了字符边界上）"
        );
    }

    /// 未超上限的说明保持原样，一个字都不能少。
    #[test]
    fn parse_latest_release_keeps_short_notes_intact() {
        let body = r#"{ "tag_name": "1.0.0", "body": "短说明" }"#;
        let r = parse_latest_release(body).unwrap();
        assert_eq!(r.notes, "短说明");
    }

    /// `describe` 两个分支的精确文案。
    #[test]
    fn describe_has_exact_wording_for_both_branches() {
        assert_eq!(
            describe("0.1.0", "0.2.1", true),
            "发现新版本 0.2.1（当前 0.1.0）"
        );
        assert_eq!(describe("0.1.0", "0.2.1", false), "已是最新版本 0.1.0");
    }

    /// `current_version()` 必须等于编译期写入的 `CARGO_PKG_VERSION`。
    #[test]
    fn current_version_matches_cargo_pkg_version() {
        assert_eq!(current_version(), env!("CARGO_PKG_VERSION"));
    }

    /// 接口 URL 的形状固定下来：写错 owner/repo 或漏掉 `/releases/latest`
    /// 都会让检查更新静默失效，这里把它钉死。
    #[test]
    fn releases_api_url_has_expected_shape() {
        assert!(RELEASES_API_URL.starts_with("https://api.github.com/repos/"));
        assert!(RELEASES_API_URL.ends_with("/releases/latest"));
    }

    /// 校验从 assets 中正确提取 exe 附件直链与大小
    #[test]
    fn parse_latest_release_extracts_asset_download_url() {
        let body = r#"{
            "tag_name": "v0.3.0",
            "assets": [
                {
                    "name": "voice2word-windows-x64.zip",
                    "browser_download_url": "https://github.com/ysg0422/voice2word/releases/download/v0.3.0/voice2word-windows-x64.zip",
                    "size": 25000000
                },
                {
                    "name": "voice2word.exe",
                    "browser_download_url": "https://github.com/ysg0422/voice2word/releases/download/v0.3.0/voice2word.exe",
                    "size": 27000000
                }
            ]
        }"#;
        let r = parse_latest_release(body).expect("带 assets 附件的 payload 必须解析成功");
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.asset_name, "voice2word.exe");
        assert_eq!(
            r.download_url,
            "https://github.com/ysg0422/voice2word/releases/download/v0.3.0/voice2word.exe"
        );
        assert_eq!(r.asset_size, 27000000);
    }

    /// 截断的下载必须被判为不完整：镜像/代理中途断流时 `read` 会以 `Ok(0)`
    /// 正常收尾，只看「读到过数据」会把残缺 exe 当成成功落盘。
    #[test]
    fn download_is_complete_detects_truncation() {
        // 未给 Content-Length（chunked）：无从校验，放行。
        assert!(download_is_complete(0, 0));
        assert!(download_is_complete(123, 0));
        // 给足或超出：算完整。
        assert!(download_is_complete(1000, 1000));
        assert!(download_is_complete(1001, 1000));
        // 少一个字节都不算完整。
        assert!(!download_is_complete(999, 1000));
        assert!(!download_is_complete(0, 1000));
    }

    /// `.exe` 附件的 `browser_download_url` 缺失时不能覆盖已选中的有效 `.zip`：
    /// 否则 `download_url` 被清空，最后回退到硬编码 URL（附件名对不上就是 404）。
    #[test]
    fn parse_latest_release_ignores_asset_without_download_url() {
        let body = r#"{
            "tag_name": "v0.4.0",
            "assets": [
                {
                    "name": "voice2word-windows-x64.zip",
                    "browser_download_url": "https://example.com/v0.4.0/voice2word-windows-x64.zip",
                    "size": 25000000
                },
                { "name": "voice2word.exe", "size": 27000000 }
            ]
        }"#;
        let r = parse_latest_release(body).expect("必须解析成功");
        assert_eq!(r.asset_name, "voice2word-windows-x64.zip");
        assert_eq!(
            r.download_url,
            "https://example.com/v0.4.0/voice2word-windows-x64.zip"
        );
        assert_eq!(r.asset_size, 25000000);
    }

    /// 所有附件都没有直链时，才回退到按 tag 拼出的硬编码 URL。
    #[test]
    fn parse_latest_release_falls_back_when_no_asset_has_url() {
        let body =
            r#"{ "tag_name": "v0.4.0", "assets": [ { "name": "voice2word.exe", "size": 1 } ] }"#;
        let r = parse_latest_release(body).unwrap();
        assert_eq!(r.asset_name, "voice2word.exe");
        assert_eq!(
            r.download_url,
            "https://github.com/ysg0422/voice2word/releases/download/v0.4.0/voice2word.exe"
        );
    }

    /// 附件名来自服务端，落盘前必须取 basename：绝对路径 / `..` 会逃出 `updates/`。
    #[test]
    fn safe_file_name_strips_directories_and_dotdot() {
        assert_eq!(safe_file_name("voice2word.exe"), "voice2word.exe");
        assert_eq!(safe_file_name("  voice2word.exe  "), "voice2word.exe");
        assert_eq!(safe_file_name(r"..\..\evil.exe"), "evil.exe");
        assert_eq!(safe_file_name("../../evil.exe"), "evil.exe");
        assert_eq!(safe_file_name(r"C:\Users\Public\x.exe"), "x.exe");
        // 空名 / 纯目录名一律回退成默认文件名
        assert_eq!(safe_file_name(""), "voice2word.exe");
        assert_eq!(safe_file_name(".."), "voice2word.exe");
        assert_eq!(safe_file_name("."), "voice2word.exe");
    }

    /// 长度校验必须把「截断」和「过小（疑似错误页）」都判为失败，且各带原因——
    /// 否则调用方删掉 `.part` 却留下 `last_err = None`，最后会报成
    /// 「所有下载镜像均不可用」，与真实原因对不上。
    #[test]
    fn verify_download_rejects_truncated_and_tiny_payloads() {
        assert!(verify_download(2000, 2000).is_ok());
        // 未给 Content-Length：只做下限校验。
        assert!(verify_download(5000, 0).is_ok());

        let truncated = verify_download(999, 1000).expect_err("截断必须判失败");
        assert!(truncated.contains("不完整"), "实际：{truncated}");

        let tiny =
            verify_download(MIN_DOWNLOAD_BYTES, MIN_DOWNLOAD_BYTES).expect_err("过小必须判失败");
        assert!(tiny.contains("字节"), "实际：{tiny}");
        assert!(verify_download(0, 0).is_err());
    }

    /// 代理返回的 HTML 错误页（HTTP 200 + 长度自洽）不能当成安装包落盘。
    #[test]
    fn magic_matches_rejects_html_error_page() {
        assert!(magic_matches("voice2word.exe", b"MZ\x90\x00\x03"));
        assert!(!magic_matches("voice2word.exe", b"<!DO"));
        assert!(magic_matches("voice2word-windows-x64.zip", b"PK\x03\x04"));
        assert!(!magic_matches("voice2word-windows-x64.zip", b"MZ\x90\x00"));
        // 未知扩展名不校验，保持既有行为。
        assert!(magic_matches("voice2word.tar.gz", b"\x1f\x8b\x08\x00"));
        assert!(!magic_matches("voice2word.exe", b""));
    }

    /// `digest` 字段的归一化：只认 `sha256:<64 位十六进制>`，大小写统一到小写。
    /// 别的算法与形状不对的串一律 `None`——宁可不校验，也不能拿不支持的算法
    /// 假装校验过。
    #[test]
    fn normalize_digest_accepts_only_sha256_hex() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            normalize_digest(&format!("sha256:{hex}")),
            Some(hex.to_string())
        );
        assert_eq!(
            normalize_digest(&format!("SHA256:{}", hex.to_uppercase())),
            Some(hex.to_string()),
            "算法名与十六进制都不区分大小写"
        );
        // 反例：不支持的算法 / 长度不对 / 含非十六进制 / 没有算法前缀
        let short = format!("sha256:{}", &hex[..62]);
        let long = format!("sha256:{hex}00");
        let non_hex =
            "sha256:zzzz456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string();
        for bad in [
            "md5:0123456789abcdef0123456789abcdef",
            short.as_str(),
            long.as_str(),
            non_hex.as_str(),
            hex,
            "",
            "sha256:",
        ] {
            assert_eq!(normalize_digest(bad), None, "{bad:?} 不该被接受");
        }
    }

    /// 摘要比对大小写不敏感（十六进制两种写法都合法）。
    #[test]
    fn digest_comparison_ignores_hex_case() {
        assert!(digests_match("abc123", "ABC123"));
        assert!(digests_match("abc123", "abc123"));
        assert!(!digests_match("abc123", "abc124"));
        assert!(!digests_match("abc123", "abc12"));
    }

    /// 流式摘要必须与「一次算完整内容」一致，否则校验会误报/漏报。
    /// 用 sha256("abc") 的已知向量钉住。
    #[test]
    fn streaming_hasher_matches_known_vector() {
        let mut hasher = StreamingHasher::new(true);
        hasher.update(b"a");
        hasher.update(b"bc");
        assert_eq!(
            hasher.finish_hex(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string())
        );
        // 未启用时不计算（上游没给摘要就不做无谓开销）
        let mut off = StreamingHasher::new(false);
        off.update(b"abc");
        assert_eq!(off.finish_hex(), None);
    }

    /// 附件元数据里的 `digest` 要被解析进 `asset_sha256`，供落盘前校验使用。
    #[test]
    fn parse_latest_release_extracts_asset_digest() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let body = format!(
            r#"{{
                "tag_name": "v0.3.0",
                "assets": [
                    {{
                        "name": "voice2word.exe",
                        "browser_download_url": "https://example.com/voice2word.exe",
                        "size": 1234,
                        "digest": "sha256:{hex}"
                    }}
                ]
            }}"#
        );
        let r = parse_latest_release(&body).expect("应解析成功");
        assert_eq!(r.asset_sha256.as_deref(), Some(hex));

        // 没有 digest（或是不支持的算法）→ None，表示「无从校验」
        let no_digest = r#"{ "tag_name": "v0.3.0", "assets": [
            { "name": "voice2word.exe", "browser_download_url": "https://e/x.exe", "digest": "md5:aa" }
        ] }"#;
        let r2 = parse_latest_release(no_digest).expect("应解析成功");
        assert_eq!(r2.asset_sha256, None);

        // 完全没有 assets → 回退 URL，同样没有摘要
        let r3 = parse_latest_release(r#"{ "tag_name": "v0.3.0" }"#).expect("应解析成功");
        assert_eq!(r3.asset_sha256, None);
    }

    /// 登记表按 URL 取回摘要：只有被官方声明过的那个 URL 才有值。
    #[test]
    fn advertised_digest_registry_is_url_scoped() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let url = "https://example.com/voice2word-digest-test/voice2word.exe";
        record_advertised_digest(url, Some(hex));
        assert_eq!(advertised_digest(url).as_deref(), Some(hex));
        // 反例：别的 URL（包括镜像派生出来的那些前缀 URL）不会命中
        assert_eq!(
            advertised_digest(&format!("https://ghproxy.net/{url}")),
            None
        );
        assert_eq!(advertised_digest("https://example.com/other.exe"), None);
        // 没有摘要时不能登记（否则会误报「摘要为空」）
        record_advertised_digest("https://example.com/no-digest.exe", None);
        assert_eq!(advertised_digest("https://example.com/no-digest.exe"), None);
        // 同 URL 再次登记以最新一次为准，且不会堆积重复项
        let other = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        record_advertised_digest(url, Some(other));
        assert_eq!(advertised_digest(url).as_deref(), Some(other));
    }
}
