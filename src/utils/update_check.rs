//! 更新检查：只判断「有没有新版」，不下载、不安装。
//!
//! # 为什么需要它
//!
//! 本程序以单个 exe 发布，用户拿到的是一份拷贝。上游发了新版本之后，用户没有任何
//! 渠道能知道——除非自己回仓库页面看。结果是用户长期停在旧版本上：已经修好的 bug
//! 还在反馈，已经填掉的坑还在踩。本模块补上「告知」这一环：向 GitHub Releases API
//! 要最新 tag，和编译进二进制的版本比较，**只有更新时**才把版本号、发行说明
//! （截断后的）和下载页交出去。
//!
//! # 它**不做**什么
//!
//! 只检查，不下载、不替换。静默替换一个正在运行的 exe 是把安装搞坏的经典做法：
//! 文件被占用导致替换失败、替换到一半断电留下半个 exe、被杀软或签名校验拦下……
//! 这些失败的代价远大于「用户自己多点一次下载页」。真正的下载与原子替换是另一件
//! 独立且风险更高的工作，不在这里做。
//!
//! # 网络与调用时机
//!
//! [`fetch_latest_release`] 会发起**一次外网请求**
//! （`GET https://api.github.com/repos/.../releases/latest`）。它只在用户**主动**
//! 点「检查更新」时被调用；界面不会后台轮询，程序启动也不会自动联网——本程序主打
//! 「完全本地运行」，偷偷联网会破坏这个承诺，还会白白撞上 GitHub 的按 IP 限流。
//!
//! 与 [`super::model_download`] 里的模型下载不同，这里**没有镜像回退**：那些镜像
//! 面向的是几十到几百 MB 的 release 附件，而这里只要一小段 JSON。失败时给出
//! 可操作的中文错误（见 [`fetch_latest_release`]），由界面决定怎么展示。
//!
//! # 结构
//!
//! 解析（[`parse_latest_release`]）、版本比较（[`is_newer`]）与文案组装
//! （[`describe`]）都是纯函数，因此能被完整单测覆盖；只有
//! [`fetch_latest_release`] 碰 IO。

use std::time::Duration;

use anyhow::{anyhow, Result};

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

    Ok(LatestRelease {
        version,
        published_at: text_field("published_at"),
        page_url: text_field("html_url"),
        notes,
    })
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
    parse_latest_release(&body)
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

    // 注意：这里**故意不给 `fetch_latest_release` 写测试**。
    // 任何测试只要真的发请求，整套单测就会依赖 GitHub 的可用性与限流额度：
    // CI 里一次 403/超时就会把和本模块无关的提交判红，而且离线环境（含
    // `cargo test --offline`）根本无法通过。IO 与解析的边界已经被
    // `parse_latest_release` 的用例覆盖，网络那一层交给手工验证。
}
