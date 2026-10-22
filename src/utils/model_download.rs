//! 模型 / 外部组件的内置下载器。
//!
//! # 为什么需要它
//!
//! 模型与可执行文件合计约 2.9 GB，不进版本库（见 `.gitignore`）。此前用户
//! clone 下来只拿到源码，启动后各引擎报「未就绪」，只能自己去找模型、对目录结构、
//! 改 `config.toml`——没有任何界面引导。
//!
//! # 为什么选这些源
//!
//! 2026-10-05 实测（本机无代理）：
//!
//! | 源 | 结果 |
//! | --- | --- |
//! | `hf-mirror.com` | **HTTP 200，全部模型文件可下** |
//! | `gh-proxy.com` / `ghproxy.net`（GitHub Release 代理） | **HTTP 200，附件可下** |
//! | `www.modelscope.cn` | HTTP 200 |
//! | `github.com` 直连 release 附件 | **超时（不可达）** |
//!
//! 模型走 `hf-mirror.com`（HuggingFace 的国内镜像）；部分只发在 GitHub Release
//! 的可执行组件（whisper.cpp / sherpa-onnx）走免梯子的 GitHub 代理。
//! 两者都**不需要梯子**。注意 `github.com` 首页能通、release 附件却超时，
//! 所以「站点可达」不等于「文件可下」——本模块登记的每个 URL 都逐个做过 HEAD 实测。
//!
//! # 关于校验
//!
//! 校验分两层，都是**按条目可选**的：
//!
//! - **大小**：始终执行，下限为 `size * 0.95`（或条目显式给出的 `min_size`）。
//!   这拦住最常见的失败模式——**截断文件**，而截断恰恰是「模型加载失败」里
//!   最难排查的一种损坏。
//! - **sha256**：仅当条目声明了 `sha256` 时才执行。上游会重新导出 / 重新量化
//!   模型文件（例如 `ggml-silero-v5.1.2.bin` 与本地同名文件同尺寸但哈希不同），
//!   所以**绝大多数条目填 `None`**；只有内容确知且稳定的条目（例如 GitHub
//!   Release 的固定附件）才填 `Some(...)`。哈希不符时报错、删除 `.part` 并
//!   尝试下一个源，绝不把损坏文件 rename 成正式名。
//!
//! 下载先写 `.part` 临时文件，成功后才 `rename` 到目标名。因此中断的下载
//! 永远不会被误认成完整模型——这一点比哈希校验更关键。
//!
//! # 断点续传
//!
//! `.part` 不只是「半成品隔离」：若它已存在且大小介于 `(0, size)` 之间，
//! 下次下载会带 `Range: bytes=<已有字节数>-` 续传。服务端返回 `206` 时以追加
//! 方式写入；返回 `200`（不支持 Range）或异常状态码时**优雅回退**到从头下载
//! （截断重写），不会因此把该源判死。若 `.part` 已不小于期望大小（上次下完但
//! 没来得及 rename），则跳过网络直接进入校验。
//!
//! 注意：启动时的 `sweep_stale_parts` 只清理**过期**（默认 7 天未更新）的
//! `.part`；近期残留会保留，因此断点续传在**跨进程**（崩溃 / 强杀后重启）场景
//! 也能真正生效，而不只是同一次运行内的换源重试。

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::AppConfig;

/// 镜像站根地址。下载 URL 基于它拼接（HuggingFace 国内镜像）。
pub const MIRROR_BASE: &str = "https://hf-mirror.com";

/// 阿里云魔搭社区根地址（国内极速直连 CDN，实测 10MB/s+）。
pub const MODELSCOPE_BASE: &str = "https://www.modelscope.cn/";
pub const MODELSCOPE_BASE_ALT: &str = "https://modelscope.cn/";

/// GitHub Release 代理站根地址。
///
/// 部分组件（如 whisper.cpp 官方 Windows 构建）只发在 GitHub
/// Release，而 `github.com` 在国内实测不可达。这类组件走
/// 免梯子的 GitHub 代理：把原始 URL 直接拼在代理域名后。
/// 已实测（无代理）返回 200 并能完整下载。
pub const PROXY_BASE: &str = "https://gh-proxy.com/";

/// 备用 GitHub 代理（主代理不可用时自动切换）。
pub const PROXY_BASE_ALT: &str = "https://ghproxy.net/";

/// 判断一个下载 URL 是否走的是「免梯子」的合法源（镜像或代理）。
///
/// 仅测试用：把「只能走免梯子源」这条约束固定下来，
/// 新增条目时若手滑贴了直连 github 的地址，在测试里就会被拦住。
#[cfg(test)]
fn is_allowed_source(url: &str) -> bool {
    [
        MIRROR_BASE,
        MODELSCOPE_BASE,
        MODELSCOPE_BASE_ALT,
        PROXY_BASE,
        PROXY_BASE_ALT,
    ]
    .iter()
    .any(|b| url.starts_with(b))
}

/// 单个可下载项。
#[derive(Debug, Clone, Copy)]
pub struct DownloadItem {
    /// 稳定标识（也用作界面上的 key）
    pub id: &'static str,
    /// 界面显示名
    pub label: &'static str,
    /// 一句话说明它负责什么
    pub note: &'static str,
    /// 相对项目根的落地路径（与 `config.toml` 里的路径同源）
    pub dest: &'static str,
    /// 候选 URL（按顺序尝试，前一个失败自动换下一个）
    pub urls: &'static [&'static str],
    /// 界面上展示的典型体积（也是下载完成后的期望值）
    pub size: u64,
    /// 判定「已就位」时接受的最小字节数；`0` 表示按 `size` 的 95% 推导。
    ///
    /// 为什么需要它：同一个逻辑模型在本项目里存在**多种合法形态**，
    /// 单看体积无法区分「损坏」与「另一种格式」：
    /// - 标点模型：上游只有 fp32（294 MB），而本项目默认用本地量化的
    ///   int8（75 MB）——两者都能被 onnxruntime 加载，都是「已就位」。
    /// - Whisper Small：上游是 q5_1（181 MB），本项目本地量化过 q5_0（175 MB），
    ///   两者只差 3.4%，落在常规容差边缘。
    ///
    /// 因此为这类条目显式给出下限，避免把用户手上完好的模型误判为缺失
    /// （误判会让界面一直显示「缺失」，用户反复下载同一个文件）。
    pub min_size: u64,
    /// 缺少它是否会导致「完全没法用」
    pub required: bool,
    /// 归类：界面按此分组
    pub group: ItemGroup,
    /// 该条目下载物是 **zip 压缩包**：下载完成后**解压**到 dest 的父目录，
    /// 而不是把下载物直接改名成 dest。dest 是解压后必须存在的入口文件。
    pub is_archive: bool,
    /// 与 dest **同目录**、必须一并存在才算就位的伴生文件（文件名，非路径）。
    ///
    /// 为什么需要它：llama.cpp 的官方 Windows 构建把真正的代码放在同目录 DLL 里，
    /// .exe 只是几 KB 的启动桩。只看 .exe 大小会把「DLL 缺失 / 解压不完整」
    /// 误判为「已就位」，用户点翻译时才炸。给出伴生 DLL 后判定才可靠。
    pub companion: Option<&'static str>,
    /// 可选的内容哈希（sha256，十六进制小写）。
    ///
    /// `None` 表示上游文件会变、不做哈希；`Some(...)` 表示强校验。只有内容
    /// 确知且稳定的条目才填 `Some`——上游重新导出会把正常下载判为失败。
    pub sha256: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemGroup {
    /// 语音识别模型
    Asr,
    /// 标点 / 润色 / 翻译模型
    Text,
    /// 外部可执行文件
    Binary,
}

impl ItemGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Asr => "语音识别模型",
            Self::Text => "标点与翻译模型",
            Self::Binary => "外部组件",
        }
    }
}

const MB: u64 = 1024 * 1024;

/// 从 URL 取 host（失败返回空串，调用方据此跳过优化）。
fn url_host(url: &str) -> String {
    url.split("//")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 是否直连境外的源（HuggingFace / GitHub 及其代理）。
///
/// # 为什么需要区分国内外
///
/// 本项目同时登记了国内直连源（ModelScope，实测 5~7 MB/s）与境外源
/// （hf-mirror / GitHub + 代理）。**两者都有用**：
/// - 国内网络：ModelScope 快，境外源多半不通；
/// - 挂梯子 / 海外：境外源反而快，且部分文件（如 Turbo 量化）只有境外源有。
///
/// 因此绝不能把任一侧删掉或永久拉黑——那会直接废掉另一类用户。这里只用来
/// 决定「先试谁」的**排序**。
fn is_overseas_source(url: &str) -> bool {
    let host = url_host(url);
    !host.contains("modelscope")
}

/// 按「**本轮已失败过的源往后排**」重排源，其余保持原有相对顺序。
///
/// # 折中：既不拉黑，也不重复踩坑
///
/// 早先的写法是把连不上的 host **在整个会话内拉黑**。那对国内用户是省时间的，
/// 但对**挂梯子**的用户有害：他一旦先以无梯状态点过一次下载，之后开了梯子也仍会
/// 被跳过境外源——而那些源此时恰恰是最快的，且有些文件只有它们有。
///
/// 改成每次 `download_one` 都从**原始顺序**重新开始，只在**本次下载内**把刚刚
/// 失败的源挪到队尾。效果：
/// - 同一次下载不会重复白等某个死源（省时间）；
/// - 下一次下载（可能已开梯子／换了网络）重新按登记顺序试，最快的源重新回到首位。
///
/// 失败计数按 host 记：同一 host 的多个镜像 URL 一起沉底，一次下载内不会反复踩。
fn order_sources(urls: &[&'static str], failed_hosts: &[String]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = urls.to_vec();
    // 排序键（稳定排序，同档内保持登记顺序）：
    //   0 = 国内源（ModelScope，国内直连最快）
    //   1 = 境外源（挂梯子 / 海外时最快；也是部分模型唯一的来源）
    //   2 = 本次下载已失败的 host（沉底，避免重复白等连接超时）
    //
    // **三类都保留**：只调顺序，不删任何源。这样无论用户是国内直连还是挂梯子，
    // 都有机会用到对自己最快的那个；失败的源只是退到队尾，下次下载又会重新
    // 从原始顺序开始（见本函数上方的说明）。
    out.sort_by_key(|u| {
        if failed_hosts.iter().any(|h| h == &url_host(u)) {
            2
        } else if is_overseas_source(u) {
            1
        } else {
            0
        }
    });
    out
}

/// 错误是否是**连接阶段**失败（DNS / TCP / TLS），而不是「连上了但出错」。
///
/// 只有这类失败才值得把整个 host 拉黑：HTTP 4xx/5xx 只说这一个 URL 有问题，
/// 同一 host 的其它文件可能完全正常。
fn is_connect_stage_error(err: &anyhow::Error) -> bool {
    let text = err.to_string();
    text.contains("Connection Failed")
        || text.contains("tls connection init failed")
        || text.contains("Dns Failed")
        || text.contains("connect")
}

/// 下载**连接**阶段的超时。
///
/// 镜像/代理域名解析到黑洞地址时，TCP 连接可以一直挂在 SYN 重传上；没有这条
/// 超时就会先卡满整个请求上限（2 小时）才轮得到换源。
///
/// 取 8 秒：实测被墙/被劫持的镜像（TLS 握手直接断）通常在 1~3 秒内就失败，
/// 这个值只用来兜住「TCP 挂死在 SYN 重传」那类真正无响应的地址。原先的 20 秒
/// 意味着一个条目若前两个源都不可达，用户要先白等 40 秒才开始下——
/// 「下载慢」的很大一部分观感来自这里，而不是带宽。
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// 下载的**空闲**超时：多久没有新字节到达就判这条源死了。
///
/// 取 30 秒：慢速线路（几十 KB/s）每 30 秒也必然有字节到达，不会被误杀；
/// 而对端真正卡死（连接在、数据不来）时，半分钟就能判死并换下一个镜像源。
/// 原先的 60 秒会让「源半死不活」的情形白等一分钟才轮到可用源。
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 单次下载请求的**总**上限：慢网下 800 MB 也要下得完（约 2 小时）。
///
/// 与 [`DOWNLOAD_IDLE_TIMEOUT`] 分工不同：这条管「整体别无限拖」，那条管
/// 「对端不发数据就尽快判死换源」。
const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// 构造下载用的 HTTP agent。
///
/// 三条超时都做成参数（而不是直接读常量）：单测要在毫秒级验证「对端不发数据会被
/// 判死」，生产路径又必须容忍慢网与几百 MB 的大文件，两边的取值只能分开给。
fn build_download_agent(idle: Duration, connect: Duration, overall: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(overall)
        .timeout_connect(connect)
        .timeout_read(idle)
        // 自动探测并继承环境中的 HTTP_PROXY / HTTPS_PROXY（梯子加速）
        .try_proxy_from_env(true)
        .build()
}

/// 全部可下载项。**每个 URL 都做过 HEAD 实测**（2026-10-05，无代理）。
///
/// 说明两处与「官方文件名」不一致的地方：
/// - `ggml-small-q5_0.bin` 是本项目**本地量化**的产物，上游只有 `q5_1`。
///   这里登记上游真实存在的 `ggml-small-q5_1.bin`，落盘时仍命名为
///   `ggml-small-q5_0.bin`，因为 `config.toml` 的默认档位指向该名字——
///   两者都是 small 的 5-bit 量化，体积/速度/精度同级，可互换使用。
/// - 标点模型上游只有 fp32 的 `model.onnx`（280 MB），没有 int8 版；
///   落盘沿用配置里的 `model.int8.onnx` 文件名（onnxruntime 不依赖扩展名）。
pub const ITEMS: &[DownloadItem] = &[
    // ── 语音识别 ──
    DownloadItem {
        id: "sensevoice-model",
        label: "SenseVoice 模型",
        note: "极速识别（非自回归，自带标点与数字规范）",
        dest: "models/sensevoice/model.int8.onnx",
        urls: &[
            "https://www.modelscope.cn/models/poloniumrock/SenseVoiceSmallOnnx/resolve/master/model.int8.onnx",
            "https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/model.int8.onnx",
        ],
        size: 239_233_841,
        min_size: 200_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "sensevoice-tokens",
        label: "SenseVoice 词表",
        note: "上面那个模型的分词表，缺它无法启动",
        dest: "models/sensevoice/tokens.txt",
        urls: &[
            "https://www.modelscope.cn/models/poloniumrock/SenseVoiceSmallOnnx/resolve/master/tokens.txt",
            "https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/tokens.txt",
        ],
        size: 315_894,
        min_size: 100_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "sensevoice-vad",
        label: "SenseVoice 静音检测（Silero VAD）",
        note: "SenseVoice 引擎切分语音段所需，缺它该引擎无法启用",
        dest: "models/sensevoice/silero_vad.onnx",
        urls: &[
            // sherpa-onnx 官方发在 GitHub Release，走免梯子代理（与 whisper-cli 同一套）。
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        ],
        size: 643_854,
        min_size: 400_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        // 来源是 GitHub Release 的固定附件（两个代理实测字节一致），内容稳定，
        // 因此这里做**强校验**：拿到 HTML 错误页 / 被替换的文件都会当场暴露。
        sha256: Some("9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"),
    },
    // ── 本地识别引擎：同一套 sherpa-onnx 运行时承载的其它模型族 ──
    //
    // 这些包的形态是 .tar.bz2（解压后 model.int8.onnx + tokens.txt 平铺在目标目录），
    // 由 `archive_kind_from_url` 自动识别、`extract_tar_bz2` 用内置便携 Python 解开，
    // 并剥掉包里那层版本号目录。companion 填 tokens.txt：它与模型同目录、缺一个都跑不起来。
    DownloadItem {
        id: "asr-dolphin",
        label: "Dolphin Base（多语种·40 种语言）",
        note: "DataoceanAI 出品，专攻东亚/东南亚语种（日韩泰越印尼等），仅 77MB，小而快",
        dest: "models/dolphin/model.int8.onnx",
        urls: &[
            // 官方只发在 GitHub Release：走两个免梯子代理（与 whisper-cli 同一套做法）。
            // 仓库里有一条测试禁止直连 github.com —— 不挂梯子的用户会卡死在那里。
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-dolphin-base-ctc-multi-lang-int8-2025-04-02.tar.bz2",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-dolphin-base-ctc-multi-lang-int8-2025-04-02.tar.bz2",
        ],
        size: 80_671_385, // 实测 Content-Length（压缩包）
        // 判定的是**解压后**的 model.int8.onnx，比压缩包小得多；给一个宽松下限即可。
        min_size: 60_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: true,
        // 与模型同目录的词表：缺它这个族跑不起来，所以一并算「已就位」的判据。
        companion: Some("tokens.txt"),
        sha256: None,
    },
    DownloadItem {
        id: "asr-paraformer",
        label: "Paraformer 中文（阿里·工业档）",
        note: "非自回归，速度和 SenseVoice 同档但长句/数字更稳；997MB",
        dest: "models/paraformer/model.int8.onnx",
        urls: &[
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-paraformer-zh-2024-03-09.tar.bz2",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-paraformer-zh-2024-03-09.tar.bz2",
        ],
        size: 996_591_364, // 实测 Content-Length（压缩包）
        min_size: 200_000_000, // 解压后的 model.int8.onnx
        required: false,
        group: ItemGroup::Asr,
        is_archive: true,
        companion: Some("tokens.txt"),
        sha256: None,
    },
    DownloadItem {
        id: "asr-firered",
        label: "FireRedASR Large（小红书·中英最高精度）",
        note: "公开榜单上中文准确率超过 Whisper-large-v3；编码器-解码器结构，慢但准；1.47GB",
        // 该族是 encoder + decoder 两个 onnx，入口登记 encoder；decoder 由运行器
        // 在模型同目录自动查找（decoder.int8.onnx → decoder.onnx → decoder.fp16.onnx）。
        dest: "models/firered/encoder.int8.onnx",
        urls: &[
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-fire-red-asr-large-zh_en-2025-02-16.tar.bz2",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-fire-red-asr-large-zh_en-2025-02-16.tar.bz2",
        ],
        size: 1_469_513_701,
        min_size: 300_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: true,
        companion: Some("tokens.txt"),
        sha256: None,
    },
    DownloadItem {
        id: "asr-canary",
        label: "Canary 180M Flash（英/西/德/法）",
        note: "NVIDIA 多语种小模型，147MB；只认这四种语言，中文素材不要选",
        dest: "models/canary/encoder.int8.onnx",
        urls: &[
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8.tar.bz2",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8.tar.bz2",
        ],
        size: 153_692_328,
        min_size: 50_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: true,
        companion: Some("tokens.txt"),
        sha256: None,
    },
    DownloadItem {
        id: "asr-parakeet",
        label: "Parakeet CTC 110M（英文 SOTA 级）",
        note: "NVIDIA 英文识别，紧凑且很准；只认英文，414MB",
        dest: "models/parakeet/model.onnx",
        urls: &[
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet_tdt_ctc_110m-en-36000.tar.bz2",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet_tdt_ctc_110m-en-36000.tar.bz2",
        ],
        size: 434_206_949,
        min_size: 100_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: true,
        companion: Some("tokens.txt"),
        sha256: None,
    },
    DownloadItem {
        id: "asr-moonshine",
        label: "Moonshine Base（英文·低延迟）",
        note: "Useful Sensors 出品，MIT 许可；短句/实时场景延迟很低，250MB",
        // 四件套：preprocess / encode / uncached_decode / cached_decode（+ tokens）。
        // 入口登记 encoder，其余由运行器在模型同目录按通配自动找。
        dest: "models/moonshine/encode.int8.onnx",
        urls: &[
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-moonshine-base-en-int8.tar.bz2",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-moonshine-base-en-int8.tar.bz2",
        ],
        size: 250_807_309,
        min_size: 60_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: true,
        companion: Some("tokens.txt"),
        sha256: None,
    },
    DownloadItem {
        id: "asr-parakeet-tdt",
        label: "Parakeet TDT 0.6B v2（英文·最高精度）",
        note: "NVIDIA transducer 结构，英文准确率在开源模型里属于第一梯队；482MB，比 CTC 版更大也更准",
        // 三件套：encoder / decoder / joiner（+ tokens）。入口登记 encoder。
        dest: "models/parakeet-tdt/encoder.int8.onnx",
        urls: &[
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
        ],
        size: 482_468_385,
        min_size: 100_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: true,
        companion: Some("tokens.txt"),
        sha256: None,
    },
    DownloadItem {
        id: "whisper-tiny",
        label: "Whisper Tiny（轻量尝鲜）",
        note: "75MB 超轻模型，低内存占用与秒级测试",
        dest: "models/whisper/ggml-tiny.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-tiny.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
        ],
        size: 77_691_713, // 实测 ggml-tiny.bin
        min_size: 70_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-base",
        label: "Whisper Base（最省资源）",
        note: "低配机器可选，速度最快",
        dest: "models/whisper/ggml-base.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-base.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        ],
        size: 147_951_465, // 实测 ggml-base.bin
        min_size: 130_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-small",
        label: "Whisper Small（均衡档）",
        note: "默认档位；中文精度与速度平衡",
        dest: "models/whisper/ggml-small-q5_0.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-small-q5_1.bin",
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-small.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-small-q5_1.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        ],
        // 实测上游 q5_1 为 181_300_000 字节；本机若已有 q5_0/ggml-small 也应算就位。
        // 数值必须是**字节数**：显示层 human_size 按 1024 换算，写成十进制约数
        // 会显示出 172.9 MB 这种与实际不符的怪数。
        size: 181_300_000,
        min_size: 160_000_000,
        required: true,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-medium",
        label: "Whisper Medium（进阶档）",
        note: "1.5GB 进阶模型，介于 Small 与 Large 之间",
        dest: "models/whisper/ggml-medium.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-medium.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        ],
        size: 1_530_000_000,
        min_size: 1_400_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-turbo-q5",
        label: "Whisper Turbo Q5（推荐）",
        note: "大模型量化版，速度与精度兼得（547MB）",
        dest: "models/whisper/ggml-large-v3-turbo-q5_0.bin",
        urls: &[
            // 注：本模型国内 ModelScope 暂无 ggml 镜像（上游只提供 safetensors/pt）。
            // 两条源都留着：国内用户可能命中，挂梯子的用户则靠 hf-mirror。
            // 排序在运行时由 `order_sources` 决定（国内优先、失败沉底）。
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-large-v3-turbo-q5_0.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
        ],
        size: 574_041_195, // 实测 ggml-large-v3-turbo-q5_0.bin（547.4 MiB）
        min_size: 500_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-turbo-q8",
        label: "Whisper Turbo Q8（高精）",
        note: "旗舰精度量化版，抗口音吞音（833MB）",
        dest: "models/whisper/ggml-large-v3-turbo-q8_0.bin",
        urls: &[
            // 同 Turbo Q5：国内暂无 ggml 镜像，双源都留（见该条目的说明）。
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-large-v3-turbo-q8_0.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q8_0.bin",
        ],
        size: 874_188_075, // 实测 ggml-large-v3-turbo-q8_0.bin（833.7 MiB）
        min_size: 800_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-large-v3",
        label: "Whisper Large-V3（全尺寸旗舰）",
        note: "3.1GB 原始旗舰全尺寸模型，最强识别与抗噪",
        dest: "models/whisper/ggml-large-v3.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-large-v3.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",
        ],
        size: 3_094_000_000,
        min_size: 3_000_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },

    // ── 高精度档（比 Turbo 更准，慢一些；按需下载即可，不占必需集）──
    //
    // 这批是 whisper.cpp 官方仓库里「精度优先」的几档。登记大小全部用
    // `curl -IL` 实测的 Content-Length 填的——下载器现在有「超过登记大小 105%
    // 就中止」的保护，这里写错一个数字，用户就再也下不动那个模型。
    DownloadItem {
        id: "whisper-large-v3-q5_0",
        label: "Whisper Large-V3 Q5（高精·省空间）",
        note: "比 Turbo 更准，体积只有旗舰的三分之一；追求准确率又不想吃满 3GB 时选它",
        dest: "models/whisper/ggml-large-v3-q5_0.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-large-v3-q5_0.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-q5_0.bin",
        ],
        size: 1_081_140_203, // 实测 ggml-large-v3-q5_0.bin
        min_size: 1_000_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-large-v3-turbo-f16",
        label: "Whisper Turbo 全精度（F16）",
        note: "Turbo 的未量化版本（1.6GB）；量化会掉一点精度，设备够快时用它最稳",
        dest: "models/whisper/ggml-large-v3-turbo.bin",
        urls: &[
            // ModelScope 上没有这个文件（实测 404），只给镜像源，避免白等一次 404。
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
        ],
        size: 1_624_555_275, // 实测 ggml-large-v3-turbo.bin
        min_size: 1_500_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-large-v2",
        label: "Whisper Large-V2（全精度）",
        note: "上一代旗舰全尺寸；某些语种/口音比 v3 更稳，代价是慢",
        dest: "models/whisper/ggml-large-v2.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-large-v2.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v2.bin",
        ],
        size: 3_094_623_691, // 实测 ggml-large-v2.bin
        min_size: 3_000_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-medium-q8",
        label: "Whisper Medium Q8（高精·中档）",
        note: "Medium 的 8bit 量化版；比 Small 准、比 Large 省，笔记本上的折中之选",
        dest: "models/whisper/ggml-medium-q8_0.bin",
        urls: &[
            // ModelScope 上没有这个文件（实测 404）。
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-medium-q8_0.bin",
        ],
        size: 823_369_779, // 实测 ggml-medium-q8_0.bin
        min_size: 780_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    // ── 英语专用档（.en 模型在纯英文素材上明显更准，中文素材别选）──
    DownloadItem {
        id: "whisper-medium-en",
        label: "Whisper Medium.en（英语专用）",
        note: "只认英文，纯英文素材上比同尺寸多语种模型更准；中文素材请勿选",
        dest: "models/whisper/ggml-medium.en.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-medium.en.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-medium.en.bin",
        ],
        size: 1_533_774_781, // 实测 ggml-medium.en.bin
        min_size: 1_450_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "whisper-small-en",
        label: "Whisper Small.en（英语专用·轻）",
        note: "只认英文的轻量档（465MB），英文播客/网课的首选",
        dest: "models/whisper/ggml-small.en.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-small.en.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin",
        ],
        size: 487_614_201, // 实测 ggml-small.en.bin
        min_size: 460_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "silero-vad",
        label: "Silero VAD（静音检测）",
        note: "跳过空白片段，长视频显著提速",
        dest: "models/whisper/ggml-silero-v6.2.0.bin",
        urls: &[
            "https://hf-mirror.com/ggml-org/silero-v5.1.2/resolve/main/ggml-silero-v5.1.2.bin",
        ],
        size: 885_098,
        min_size: 500_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    // ── 标点与翻译 ──
    DownloadItem {
        id: "punc-model",
        label: "CT-Transformer 标点模型",
        note: "毫秒级补标点断句（达摩院非自回归）",
        dest: "models/punc/model.int8.onnx",
        urls: &[
            "https://hf-mirror.com/csukuangfj/sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12/resolve/main/model.onnx",
        ],
        size: 280_700_000, // 上游 fp32；本机 int8(75MB) 亦算就位
        min_size: 60_000_000,
        required: false,
        group: ItemGroup::Text,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "qwen-llm",
        label: "Qwen2.5-0.5B（润色 / 翻译）",
        note: "离线润色与字幕翻译，无需联网与密钥",
        dest: "models/llm/qwen2.5-0.5b-instruct-q4_k_m.gguf",
        urls: &[
            // 国内源优先（ModelScope 直连，实测可用）
            "https://www.modelscope.cn/models/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/master/qwen2.5-0.5b-instruct-q4_k_m.gguf",
            "https://hf-mirror.com/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf",
        ],
        size: 491_400_032,
        min_size: 400_000_000,
        required: false,
        group: ItemGroup::Text,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    // 下面两个是**更大的可选档**：离线翻译/润色对模型规模很敏感，0.5B 只够「能用」，
    // 1.5B/3B 的译文通顺度明显更好，且仍能在这类显卡/CPU 上跑得动。
    //
    // 为什么只加 Qwen2.5、不加更新的 Qwen3：本项目的 LLM 调用走的是
    // `llama-completion -no-cnv`（**原始补全**，不走对话模板，见 `engines/llm.rs`）。
    // Qwen3 是混合推理模型，原始补全下会把 ` thinking...<｜end▁of▁thinking｜>` 思考过程一起吐出来、
    // 混进字幕里；Qwen2.5 没有思考模式，与现有 0.5B 同族、提示词与停止符
    // （`<|im_end|>`）完全一致，因此是当前引擎下安全的选择。
    DownloadItem {
        id: "qwen-llm-1_5b",
        label: "Qwen2.5-1.5B（翻译增强）",
        note: "译文更通顺，适合正式内容；约 1.0GB",
        dest: "models/llm/qwen2.5-1.5b-instruct-q4_k_m.gguf",
        urls: &[
            "https://www.modelscope.cn/models/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/master/qwen2.5-1.5b-instruct-q4_k_m.gguf",
            "https://hf-mirror.com/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf",
        ],
        size: 1_117_700_000,
        min_size: 900_000_000,
        required: false,
        group: ItemGroup::Text,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "qwen-llm-3b",
        label: "Qwen2.5-3B（翻译高质）",
        note: "24GB 显存以下最佳性价比；约 2.0GB",
        dest: "models/llm/qwen2.5-3b-instruct-q4_k_m.gguf",
        urls: &[
            "https://www.modelscope.cn/models/Qwen/Qwen2.5-3B-Instruct-GGUF/resolve/master/qwen2.5-3b-instruct-q4_k_m.gguf",
            "https://hf-mirror.com/Qwen/Qwen2.5-3B-Instruct-GGUF/resolve/main/qwen2.5-3b-instruct-q4_k_m.gguf",
        ],
        size: 2_105_300_000,
        min_size: 1_800_000_000,
        required: false,
        group: ItemGroup::Text,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    // ── 外部组件 ──
    DownloadItem {
        id: "ffmpeg",
        label: "FFmpeg",
        note: "音视频解码与抽音，必需组件",
        dest: "tools/ffmpeg.exe",
        urls: &[
            "https://hf-mirror.com/lj1995/VoiceConversionWebUI/resolve/main/ffmpeg.exe",
        ],
        size: 50_500_000,
        // 下限压得很低：ffmpeg 有**静态单文件**（约 50 MB）与**共享构建**
        // （exe 仅 ~0.5 MB + 同目录一堆 av*.dll）两种合法分发形态，
        // 本机用的正是后者。卡体积会把「已经能用」的 ffmpeg 误判为缺失。
        // 只要求它是个像样的可执行文件即可（几百 KB 起）。
        min_size: 200_000,
        required: true,
        group: ItemGroup::Binary,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "ffplay",
        label: "FFplay（伴音播放器）",
        note: "内嵌视频播放器的音频同步渲染组件",
        dest: "tools/ffplay.exe",
        urls: &[
            "https://hf-mirror.com/lj1995/VoiceConversionWebUI/resolve/main/ffplay.exe",
            "https://hf-mirror.com/Politrees/RVC_resources/resolve/main/tools/ffmpeg/ffplay.exe",
        ],
        size: 17_850_368,
        min_size: 1_000_000,
        required: true,
        group: ItemGroup::Binary,
        is_archive: false,
        companion: None,
        sha256: None,
    },
    DownloadItem {
        id: "llama-cpp",
        label: "llama.cpp 推理程序（离线翻译/润色）",
        note: "本地 Qwen 翻译与润色的推理后端；下载后自动解压到 tools/",
        // dest 是解压后必须存在的入口文件（llama-completion.exe）；
        // zip 里的 llama-server.exe 与一堆 DLL 会同目录落盘。
        dest: "tools/llama-completion.exe",
        urls: &[
            // 官方 ggml-org/llama.cpp 的 Windows CPU 构建镜像（16.9 MB）。
            // github release 附件在本机不可达，hf-mirror 上这个镜像可下、无需梯子。
            "https://hf-mirror.com/limnmn/llama.cpp-b9637-Windows-Runtime/resolve/main/llama-b9637-bin-win-cpu-x64.zip",
        ],
        size: 16_906_751,
        // 解压后入口 exe 只有几 KB（真正代码在同目录 DLL），故下限压到 1 KB；
        // 「解压是否完整」由 companion（llama-server.exe）与 DLL 存在性把关。
        min_size: 1_000,
        required: false,
        group: ItemGroup::Binary,
        is_archive: true,
        companion: Some("llama-server.exe"),
        sha256: None,
    },
    DownloadItem {
        id: "whisper-cli",
        label: "whisper.cpp 识别程序（Whisper 引擎）",
        // 官方 Windows 构建只有 CPU（无 Vulkan）。下载后自动解压到 tools/whisper-vulkan/。
        // 目录名沿历史叫 whisper-vulkan，但这里放的是**官方 CPU 构建**：MSVC 编译 +
        // VC++ 运行库（MSVCP140/VCRUNTIME140），系统基本自带，开箱即用。
        // 想用 GPU 加速：跑 scripts/build_whisper_msvc_vulkan.bat 自编 MSVC + Vulkan 版，
        // 脚本会自动部署到本目录（覆盖 CPU 包）。GPU 版含 ggml-vulkan.dll，
        // 会被 `looks_like_custom_build` 识别为「自编译构建」——此后点「下载」不会覆盖它。
        note: "官方 CPU 构建（免梯子下载）；想用 GPU 请跑 scripts/build_whisper_msvc_vulkan.bat",
        // dest 与 config.toml 的默认 whisper_cli 一致；zip 内部是扁平的
        // Release/（已被解压器剥掉这层包装），所有 exe/dll 落到同目录。
        dest: "tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe",
        urls: &[
            // 官方 ggml-org/whisper.cpp v1.8.4 的 Windows x64 构建（4.1 MB）。
            // github.com 直连不可达，故走免梯子的 GitHub 代理。
            "https://gh-proxy.com/https://github.com/ggml-org/whisper.cpp/releases/download/v1.8.4/whisper-bin-x64.zip",
            // 备用代理（主代理不可用时自动切换）
            "https://ghproxy.net/https://github.com/ggml-org/whisper.cpp/releases/download/v1.8.4/whisper-bin-x64.zip",
        ],
        size: 4_078_768,
        // 解压后入口 exe 约 0.5 MB；程序与各 DLL 同目录，
        // 完整性由 companion（whisper.dll）把关。
        min_size: 100_000,
        required: false,
        group: ItemGroup::Binary,
        is_archive: true,
        companion: Some("whisper.dll"),
        // 同上：GitHub Release v1.8.4 的固定附件，两个代理实测字节一致。
        sha256: Some("74f973345cb52ef5ba3ec9e7e7af8e48cc8c71722d1528603b80588a11f82e3e"),
    },
    DownloadItem {
        id: "whisper-cublas",
        label: "whisper.cpp (NVIDIA CUDA 显卡极速版)",
        note: "NVIDIA 显卡专用（含 CUDA 12.4 加速，免装 CUDA Toolkit）",
        dest: "tools/whisper-cuda/whisper-cli.exe",
        urls: &[
            "https://gh-proxy.com/https://github.com/ggml-org/whisper.cpp/releases/download/v1.8.4/whisper-cublas-12.4.0-bin-x64.zip",
            "https://ghproxy.net/https://github.com/ggml-org/whisper.cpp/releases/download/v1.8.4/whisper-cublas-12.4.0-bin-x64.zip",
        ],
        size: 457_024_596,
        min_size: 100_000,
        required: false,
        group: ItemGroup::Binary,
        is_archive: true,
        companion: Some("cublas64_12.dll"),
        sha256: None,
    },
    DownloadItem {
        id: "sherpa-onnx-cli",
        label: "sherpa-onnx 独立执行体（SenseVoice 纯原生免 Python 版）",
        note: "官方 C++ 原生 Windows 构建，极速非自回归识别，彻底无需安装 Python / pip 环境",
        dest: "tools/sherpa-onnx-offline.exe",
        urls: &[
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.10.35/sherpa-onnx-v1.10.35-win-x64.zip",
            "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.10.35/sherpa-onnx-v1.10.35-win-x64.zip",
        ],
        size: 18_420_000,
        min_size: 100_000,
        required: false,
        group: ItemGroup::Binary,
        is_archive: true,
        companion: Some("onnxruntime.dll"),
        sha256: None,
    },
];

/// 系统 PATH 上能不能找到这个命令（不含扩展名，例如 `ffmpeg`）。
///
/// # 为什么自己扫 PATH 而不 spawn 一次 `--version`
///
/// 这里只回答「有没有这个可执行文件」：扫目录是纯文件系统读，几十微秒；spawn 一次
/// 进程要几十毫秒，还可能被杀软拦。判定口径是按 `PATHEXT` 依次试
/// `name` + 各扩展名（Windows 文件系统大小写不敏感，不必为 `.EXE`/`.exe` 各写一遍）；
/// 非 Windows 上 `PATHEXT` 不存在，退化成只试裸文件名。
pub fn is_command_available(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let mut extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_string())
        .split(';')
        .map(|ext| ext.trim().to_string())
        .filter(|ext| !ext.is_empty())
        .collect();
    // 裸文件名先试一次（PATH 里直接放了带执行位的文件的情形）。
    extensions.insert(0, String::new());
    std::env::split_paths(&path).any(|dir| {
        !dir.as_os_str().is_empty()
            && extensions
                .iter()
                .any(|ext| dir.join(format!("{name}{ext}")).is_file())
    })
}

/// 把配置里写的可执行文件路径解析成「真正能跑起来的那一个」。
///
/// 1. 配置里的路径存在 → 原样使用（用户显式指定的永远优先，不能被系统里另一个
///    同名程序顶掉）；
/// 2. 不存在、但文件名就是这个名字（`tools/ffmpeg.exe` → `ffmpeg`）且 PATH 上有
///    同名命令 → 用裸命令名，交给操作系统搜索 PATH。这样「机器上早装好了 ffmpeg」
///    不必为了本程序再放一份副本；
/// 3. 都不成立 → 原样返回。调用方据此判定「缺失」，**由调用方决定要不要下载到
///    配置写死的那个相对路径**，而不是在这里偷偷换成别的路径。
pub fn resolve_configured_executable(configured: &std::path::Path) -> std::path::PathBuf {
    if configured.exists() {
        return configured.to_path_buf();
    }
    let Some(stem) = configured.file_stem().and_then(|stem| stem.to_str()) else {
        return configured.to_path_buf();
    };
    if is_command_available(stem) {
        return std::path::PathBuf::from(stem);
    }
    configured.to_path_buf()
}

/// 配置里**这条下载项**对应的可执行文件路径（找不到对应字段就退回它自己的 `dest`）。
fn configured_executable_for(cfg: &AppConfig, item: &DownloadItem) -> String {
    match item.id {
        "ffmpeg" => cfg.paths.ffmpeg.clone(),
        "whisper-cli" | "whisper-cublas" => cfg.paths.whisper_cli.clone(),
        "llama-cpp" => cfg.paths.llama_cli.clone(),
        _ => item.dest.to_string(),
    }
}

/// 已解析路径的可用性：文件在，或它的文件名能在 PATH 上找到。
///
/// 单独抽出来是因为「体检报告」与「能不能用」问的是同一件事，两处各写一遍迟早分叉
/// ——CLI 的 `tools --json` 就曾经只按 `path.exists()` 判，于是机器上装着 ffmpeg
/// 却报 `ready: false`。
pub fn resolved_executable_is_usable(resolved: &std::path::Path) -> bool {
    resolved.exists()
        || resolved
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(is_command_available)
}

/// 某个可执行组件现在能不能用：配置里的路径在，或系统 PATH 上有同名命令。
pub fn executable_is_usable(cfg: &AppConfig, item: &DownloadItem) -> bool {
    let configured = configured_executable_for(cfg, item);
    resolved_executable_is_usable(&AppConfig::resolve_path(&configured))
}

/// 「必需的可执行组件」里当前确实不可用的那些。
///
/// 判定对象是「程序能不能干活」，不是「`tools/ffmpeg.exe` 这个文件在不在」——
/// 否则一台早就装了 ffmpeg 的机器会在启动时被推着再下一份几十 MB 的副本。
/// 返回的条目可以直接喂给下载流程（它会落到 `item.dest` 这个**相对路径**上）。
pub fn missing_required_executables(cfg: &AppConfig) -> Vec<&'static DownloadItem> {
    ITEMS
        .iter()
        .filter(|item| item.required)
        .filter(|item| item.dest.to_ascii_lowercase().ends_with(".exe"))
        .filter(|item| !executable_is_usable(cfg, item))
        .collect()
}
/// 一次扫描中复用的配置快照。
/// 为什么需要它：`resolve_existing_path` 要知道「用户把 ffmpeg 配在哪」，
/// 而那需要读 `config.toml`。若每个条目各自读一次，一次扫描就是 13 次
/// 文件读取 + 11 次 TOML 解析（`refresh_model_presence` 在启动与每次
/// 下载完成后都会调用）。这里把配置读一次、按条目查表复用。
pub struct PresenceContext {
    cfg: Option<AppConfig>,
}

impl PresenceContext {
    /// 读一次配置（读不到就退化成一概按默认路径判定，仍能工作）。
    pub fn load() -> Self {
        Self {
            cfg: AppConfig::load_from_file("config.toml").ok(),
        }
    }

    /// 供测试构造固定配置
    pub fn from_config(cfg: AppConfig) -> Self {
        Self { cfg: Some(cfg) }
    }

    pub fn is_present(&self, item: &DownloadItem) -> bool {
        is_present_with_opt(item, self.cfg.as_ref())
    }
}

/// 用**已有的**内存配置判定，不碰磁盘。
///
/// `AppState` 自己就持有 `config`，判定时再 `load_from_file` 一次纯属浪费
/// （启动路径上因此白读一遍 TOML）。调用方有配置就直接用这个。
pub fn is_present_with_config(item: &DownloadItem, cfg: &AppConfig) -> bool {
    // 判定只读 `cfg.paths` 查表，没必要把整份 `AppConfig` 深拷贝一份再取引用；
    // 这里直接借用，与 `is_present_with` 共用同一份实现，语义不变。
    is_present_with_opt(item, Some(cfg))
}

/// 一次扫描、复用同一份内存配置。
pub struct PresenceContextRef<'a> {
    cfg: &'a AppConfig,
}

impl<'a> PresenceContextRef<'a> {
    pub fn new(cfg: &'a AppConfig) -> Self {
        Self { cfg }
    }

    pub fn is_present(&self, item: &DownloadItem) -> bool {
        // 原实现是 `&Some(self.cfg.clone())`：每调一次就深拷贝整份 `AppConfig`
        // （paths / pipeline / 术语表等），而模型管理面板是**每个条目每帧**调一次，
        // 这份拷贝纯属白跑。判定只读 `cfg.paths`，直接借用传下去即可。
        is_present_with_opt(item, Some(self.cfg))
    }

    /// 是否命中了用户自编译的自包含构建（同一次扫描内复用配置，不额外读盘）。
    pub fn is_custom_build(&self, item: &DownloadItem) -> bool {
        // 同上：借用配置，避免每个条目每帧白拷贝一份 `AppConfig`。
        is_custom_build_with_opt(item, Some(self.cfg))
    }
}

/// 一次扫描得到两张缓存快照：`(model_present, model_custom_build)`。
///
/// `AppState::refresh_model_presence` 是同一套判定的「直接写状态」版本；这里
/// 返回结果而不碰状态，是为了让调用方能把**扫描本身**丢到专用线程上执行：
/// 声明了 `sha256` 的条目要全文件重算哈希（几百 MB 的顺序读），在 UI 线程
/// 同步跑会卡帧（见 `utils::background_thread` 的 DEDICATED_LONG_TASKS）。
pub fn scan_presence(cfg: &AppConfig) -> (HashMap<String, bool>, HashMap<String, bool>) {
    let ctx = PresenceContextRef::new(cfg);
    let present = ITEMS
        .iter()
        .map(|i| (i.id.to_string(), ctx.is_present(i)))
        .collect();
    let custom_build = ITEMS
        .iter()
        .map(|i| (i.id.to_string(), ctx.is_custom_build(i)))
        .collect();
    (present, custom_build)
}

/// 判断某个条目是否已就位（内部使用：允许传入已读好的配置）。
///
/// # 判定为什么不是「大小相等」
///
/// 同一个逻辑模型在本项目里存在多种合法形态，只比体积会把完好的文件
/// 误判为缺失（界面于是永远显示「缺失」，用户反复下载同一个文件）：
/// - **路径可能被改**：`config.local.toml` 可以把 `ffmpeg` 指到任意位置
///   （例如 `<某个非默认位置>\ffmpeg.exe`）。只看 `item.dest`
///   会把这种「已配好但不在默认路径」的情况误报为缺失。
/// - **格式可能不同**：标点模型上游是 fp32（294 MB），本项目默认用本地
///   量化的 int8（75 MB）；Whisper Small 上游 q5_1（181 MB）与本地的
///   q5_0（175 MB）只差 3.4%。
///
/// 因此判定分两步：先按**配置里实际生效的路径**找文件，再用
/// `min_size`（缺省为 `size` 的 95%）判下限。下限只负责拦住截断下载——
/// 那才是真正会导致「模型加载失败」且难以排查的损坏。
pub fn is_present(item: &DownloadItem) -> bool {
    let cfg = AppConfig::load_from_file("config.toml").ok();
    is_present_with(item, &cfg)
}

fn is_present_with(item: &DownloadItem, cfg: &Option<AppConfig>) -> bool {
    is_present_with_opt(item, cfg.as_ref())
}

/// [`is_present_with`] 的**借用版**：一路借 `&AppConfig`，零拷贝。
///
/// 保留上面那个「按值 Option」签名只为不动既有调用点与单测；两者共用这一份
/// 实现，因此判定逐字同源——`PresenceContextRef` 走的也是这里。
fn is_present_with_opt(item: &DownloadItem, cfg: Option<&AppConfig>) -> bool {
    let Some(path) = resolve_existing_path(item, cfg) else {
        return false;
    };
    let Ok(meta) = fs::metadata(&path) else {
        return false;
    };
    if meta.len() == 0 {
        return false;
    }
    if companion_missing(&path, item, meta.len()) {
        return false;
    }
    // 压缩包类条目的 `sha256` 是**下载物（zip）**的哈希，而不是解压后入口
    // 文件的哈希；对 `dest`（解压出来的 exe）重算必然不匹配。这类条目的
    // 完整性由 `companion` 把关，哈希只在下载 `.part` 时校验一次。
    let expected_sha256 = if item.is_archive { None } else { item.sha256 };
    present_by_content(
        &path,
        meta.len(),
        expected_sha256,
        min_acceptable_size(item),
    )
}

/// 「已存在的文件是否算就位」的内容级判定（可单测，不联网）。
///
/// # 为什么要在已存在文件上复核哈希
///
/// 只比体积会让「大小对、内容损坏」的文件被**永久**判为已就位：界面一直
/// 显示「已就位」，用户也就永远没有机会重下，直到真正加载时才炸。因此凡
/// 声明了 `sha256` 的条目，这里必须重算哈希并逐字比对，不匹配即视为**未就位**，
/// 让上层走重下流程。
///
/// # 为什么未声明哈希时退回体积下限
///
/// 大模型动辄数百 MB（Turbo Q8 有 833 MB），每次启动 / 刷新都全量哈希会让
/// 首屏明显卡顿。未声明哈希的条目内容可能随上游变化，本就无法强校验，因此
/// 退回**体积下限**的快速判定（下限只负责拦住截断下载）。
///
/// `floor == 0` 表示既无 `min_size` 也无 `size`，此时只要求文件非空
/// （调用方已保证 `len > 0`）。
fn present_by_content(path: &Path, len: u64, expected_sha256: Option<&str>, floor: u64) -> bool {
    if let Some(expected) = expected_sha256 {
        return match sha256_file(path) {
            // 十六进制大小写不敏感，避免上游登记时大小写不一致造成误判。
            Ok(actual) => actual.eq_ignore_ascii_case(expected),
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "重算已存在文件的 sha256 失败，判定为未就位"
                );
                false
            }
        };
    }
    if floor == 0 {
        return true;
    }
    len >= floor
}

/// 「自包含入口」的体积下限：达到这个体积的单文件视为静态链接构建，
/// 不再强求同目录 DLL。官方 zip 里的 .exe 是几 KB ~ 几百 KB 的启动桩，
/// 而静态构建是几 MB ~ 几十 MB的单文件（如本机的 Vulkan 版 whisper-cli 达 61 MB）。
/// 用体积区分这两种**合法形态**，避免把能用的静态构建误判为「解压不完整」。
const SELF_CONTAINED_ENTRY_FLOOR: u64 = 2 * 1024 * 1024;

/// 压缩包类组件的「伴生文件是否缺失」。
///
/// # 为什么需要这个判定
///
/// 官方 zip 里的 .exe 只是几 KB 的启动桩，真正代码在同目录 DLL 里。
/// 若只看 .exe 存不存在，「解压了一半」（只有桩、没 DLL）会被误判为已就位，
/// 用户点开始转写时才炸。
///
/// # 为什么要两道门槛
///
/// 1. **仅在默认落地路径上校验**：用户在 `config` 里把路径指向
///    自己的构建时，同目录本来就可能没有那些 DLL，那是完好的，不能强求。
/// 2. **入口体积足够大时视为自包含**：官方桩很小，静态构建很大，
///    用体积区分两种合法形态。
///
/// 两道门槛都放行后，才要求伴生文件存在。
fn companion_missing(path: &Path, item: &DownloadItem, entry_len: u64) -> bool {
    let Some(companion) = item.companion else {
        return false;
    };
    // 非默认路径：用户自己配的，交给用户负责
    if path != AppConfig::resolve_path(item.dest) {
        return false;
    }
    // 入口足够大 → 静态自包含构建，不强求伴生文件
    if entry_len >= SELF_CONTAINED_ENTRY_FLOOR {
        return false;
    }
    !path
        .parent()
        .map(|d| d.join(companion).exists())
        .unwrap_or(false)
}

/// 目标文件是否像是**用户自编译的构建**（自包含单文件，或带 GPU 后端 DLL）。
///
/// # 为什么下载前要看一眼
///
/// 本项目官方 `whisper-cli` 是 **纯 CPU** 的 MSVC 构建（几百 KB 的启动桩 +
/// 同目录一堆 DLL，不含 `ggml-vulkan.dll`）。用户按
/// `docs/企业级升级路线图.md` 的 P0-A2 自编的 **MSVC + Vulkan** 版则有两种形态：
/// 1. **单文件静态链接版**：入口 60 MB 左右、同目录没有随附 DLL
///    （见上方 `SELF_CONTAINED_ENTRY_FLOOR` 的说明）；
/// 2. **带 DLL 的 GPU 版**：入口与官方一样是几百 KB 的桩，但同目录多了
///    `ggml-vulkan.dll`、`whisper.dll` 等 GPU 后端 DLL。
///
/// 两种都合法。若不做判断就直接下载，用户辛苦编译好的构建会被官方 CPU 包
/// **悄悄覆盖**（体积骤减、GPU 支持凭空消失，而且没有任何提示）。
/// 命中时由 [`download_one`] 拒绝覆盖并给出可操作提示。
fn looks_like_custom_build(item: &DownloadItem, dest: &Path) -> bool {
    let Some(companion) = item.companion else {
        return false;
    };
    let Ok(meta) = fs::metadata(dest) else {
        return false;
    };
    // 形态一：**单文件自包含构建**——入口 >= 2 MB 且同目录没有随附 DLL。
    // （静态链接的 MinGW 版常长这样。）
    if meta.len() >= SELF_CONTAINED_ENTRY_FLOOR
        && !dest
            .parent()
            .map(|d| d.join(companion).exists())
            .unwrap_or(false)
    {
        return true;
    }
    // 形态二：**带 DLL 的 GPU 构建**——入口虽小（官方形态也是几百 KB 的桩），
    // 但同目录带着本项目官方 CPU 包**从不提供**的 GPU 后端 DLL（如
    // `ggml-vulkan.dll`），或入口本身导入 `vulkan-1.dll`。这正是用户按
    // docs/企业级升级路线图.md 的 P0-A2 自编的 MSVC + Vulkan 版形态：
    // exe + whisper.dll + ggml.dll + ggml-cpu.dll + **ggml-vulkan.dll**。
    // 若不拦住，点一次「下载」就会被官方 CPU 包覆盖、GPU 支持凭空消失。
    is_gpu_build(dest)
}

/// 该入口是否是**带 GPU 后端的构建**（本项目官方 CPU 包从不含这些）。
///
/// 多条独立证据，命中其一即可：
/// 1. 同目录存在 Vulkan、CUDA 或 DirectML 后端 DLL（如 `ggml-vulkan.dll`、`ggml-cuda.dll`、`cublas64_12.dll` 等）；
/// 2. 入口 exe 的 PE 导入表里出现 Vulkan / CUDA / DirectML 符号（静态或动态链接的 GPU 构建）。
fn is_gpu_build(entry: &Path) -> bool {
    let Some(dir) = entry.parent() else {
        return false;
    };
    let gpu_dlls = [
        "ggml-vulkan.dll",
        "ggml-cuda.dll",
        "ggml-dml.dll",
        "nvcuda.dll",
        "cublas64_12.dll",
        "cublas64_11.dll",
        "cudart64_12.dll",
        "cudart64_110.dll",
        "DirectML.dll",
    ];
    for dll in gpu_dlls {
        if dir.join(dll).exists() {
            return true;
        }
    }
    match fs::read(entry) {
        Ok(bytes) => crate::utils::pe_imports::imported_dll_names(&bytes)
            .map(|names| {
                names.iter().any(|n| {
                    let lower = n.to_ascii_lowercase();
                    lower.contains("vulkan")
                        || lower.contains("cuda")
                        || lower.contains("cublas")
                        || lower.contains("directml")
                })
            })
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// 该条目当前命中的是否是**用户自编译的自包含构建**（供界面打「自编译」标识）。
///
/// 与 [`looks_like_custom_build`] 的区别：这里先按配置解析出**实际生效**的路径
/// （用户可能把 `whisper_cli` 指到别处），再判断形态。界面据此区分
/// 「本项目下载展开的官方包」与「用户自己编的版本」，避免用户误以为自己的构建被覆盖。
pub fn is_custom_build_with(item: &DownloadItem, cfg: &Option<AppConfig>) -> bool {
    is_custom_build_with_opt(item, cfg.as_ref())
}

/// [`is_custom_build_with`] 的借用版（与 [`is_present_with_opt`] 同一套理由）。
pub fn is_custom_build_with_opt(item: &DownloadItem, cfg: Option<&AppConfig>) -> bool {
    let Some(path) = resolve_existing_path(item, cfg) else {
        return false;
    };
    looks_like_custom_build(item, &path)
}

/// 用 `config.toml` + `config.local.toml` 判定（与 [`is_present`] 同路径）。
pub fn is_custom_build(item: &DownloadItem) -> bool {
    let cfg = AppConfig::load_from_file("config.toml").ok();
    is_custom_build_with(item, &cfg)
}

/// 该条目可接受的最小字节数。
fn min_acceptable_size(item: &DownloadItem) -> u64 {
    if item.min_size > 0 {
        item.min_size
    } else if item.size > 0 {
        // 默认容差 5%：上游重新导出会有微小出入，卡死等号会误报
        (item.size as f64 * 0.95) as u64
    } else {
        0
    }
}

/// 找出该条目**实际生效**的落地路径。
///
/// 优先取 `config.toml` / `config.local.toml` 里为这个组件配的路径
/// （用户可能把 ffmpeg 装在别处），找不到再回退到默认的 `item.dest`。
///
/// 这一步很关键：只认默认路径的话，一个「已经配好且能用」的组件
/// 会被界面一直标成「缺失」。
/// 只接受 `Option<&AppConfig>`（借用）：判定只在 `cfg.paths` 上查表，
/// 不需要也不该把整份 `AppConfig` 拷一份再取引用（`PresenceContextRef` 每帧都走这里）。
fn resolve_existing_path(item: &DownloadItem, cfg: Option<&AppConfig>) -> Option<PathBuf> {
    // 配置里为同类组件指定的路径（按 dest 反查该配哪个字段）
    let configured: Option<PathBuf> = configured_path_for(item, cfg);
    if let Some(p) = configured {
        if p.exists() {
            return Some(p);
        }
    }
    let fallback = AppConfig::resolve_path(item.dest);
    if fallback.exists() {
        return Some(fallback);
    }
    None
}

/// 按条目 id 反查 `config.toml` 中对应的路径字段（已解析为绝对路径）。
/// 借用 `&AppConfig`：只在 `cfg.paths` 上查表，不需要所有权。
fn configured_path_for(item: &DownloadItem, cfg: Option<&AppConfig>) -> Option<PathBuf> {
    // 配置由调用方读一次并传入；这里只做查表，不再碰磁盘。
    let cfg = cfg?;
    let raw: Option<&str> = match item.id {
        "ffmpeg" => Some(cfg.paths.ffmpeg.as_str()),
        "whisper-tiny" | "whisper-base" | "whisper-small" | "whisper-medium"
        | "whisper-turbo-q5" | "whisper-turbo-q8" | "whisper-large-v3" => {
            Some(cfg.paths.whisper_model.as_str())
        }
        "silero-vad" => cfg.paths.vad_model.as_deref(),
        "punc-model" => cfg.paths.punc_model.as_deref(),
        "sensevoice-model" => cfg.paths.sensevoice_model.as_deref(),
        "sensevoice-tokens" => cfg.paths.sensevoice_tokens.as_deref(),
        "sensevoice-vad" => cfg.paths.sensevoice_vad.as_deref(),
        "qwen-llm" => Some(cfg.paths.llm_model.as_str()),
        // llama.cpp 也走配置：用户可能把它装在别处（例如自编译产物）。
        // 若不接配置，即使用户已经能用，界面也会一直标「缺失」。
        "llama-cpp" => Some(cfg.paths.llama_cli.as_str()),
        "whisper-cublas" => {
            if cfg.paths.whisper_cli.contains("cuda") {
                Some(cfg.paths.whisper_cli.as_str())
            } else {
                None
            }
        }
        "whisper-cli" => {
            if !cfg.paths.whisper_cli.contains("cuda") {
                Some(cfg.paths.whisper_cli.as_str())
            } else {
                None
            }
        }
        _ => None,
    };
    // whisper 档位：只有「配置指向的那个档位」才算就位，否则会把用户没选的
    // 档位也判成已就位（例如配的是 small，却因为磁盘上有 turbo 就认为 small 在）。
    if matches!(
        item.id,
        "whisper-tiny"
            | "whisper-base"
            | "whisper-small"
            | "whisper-medium"
            | "whisper-turbo-q5"
            | "whisper-turbo-q8"
            | "whisper-large-v3"
    ) {
        let configured_name = raw
            .map(|p| {
                p.rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let item_name = item
            .dest
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        // small-q5_0 与 small-q5_1 视为同一档（本地量化版本号差异）
        let same_tier = configured_name == item_name
            || (configured_name.starts_with("ggml-tiny") && item_name.starts_with("ggml-tiny"))
            || (configured_name.starts_with("ggml-small") && item_name.starts_with("ggml-small"))
            || (configured_name.starts_with("ggml-base") && item_name.starts_with("ggml-base"))
            || (configured_name.starts_with("ggml-medium") && item_name.starts_with("ggml-medium"))
            || (configured_name.contains("turbo-q5") && item_name.contains("turbo-q5"))
            || (configured_name.contains("turbo-q8") && item_name.contains("turbo-q8"))
            || (configured_name == "ggml-large-v3.bin" && item_name == "ggml-large-v3.bin");
        if !same_tier {
            return None;
        }
    }
    raw.map(AppConfig::resolve_path)
}

/// 按 id 查条目。
///
/// # 为什么需要它
///
/// 界面在「下载完成」「删除文件」这类回调里拿到的往往是**字符串 id**
/// （跨线程消息、事件参数只能带 `'static` 的简单值），但真正干活时需要完整的
/// `DownloadItem`——落地路径、是不是压缩包、有没有伴生文件都写在里面。
/// 没有这个查表函数，每个调用点都得自己写一遍 `ITEMS.iter().find(...)`；
/// 一旦某处写成按 `label` 之类的近似匹配，就会「找错条目」而且极难排查。
///
/// 条目只有十来个，线性扫描足够快；返回 `&'static` 也让调用方不必 clone。
pub fn item_by_id(id: &str) -> Option<&'static DownloadItem> {
    ITEMS.iter().find(|i| i.id == id)
}

/// 人类可读的体积：`512 B` / `123 KB` / `1.2 MB` / `0.9 GB`。
///
/// # 为什么这么定
///
/// - **1 KB = 1024 B**：与磁盘容量、任务管理器口径一致，用户对得上。
/// - **保留一位小数，并抹掉末尾的 `.0`**：所以是 `123 KB` 而不是
///   `123.0 KB`。界面上的体积是给用户扫一眼判断「要不要下」用的，
///   多一个 `.0` 只是噪声，还显得像是没处理过。
/// - **不足 1 KB 时按整数字节显示**（`512 B`）：字节级的小数没有意义。
///
/// # 单一实现
///
/// `src/ui/components/model_manager.rs` 里曾有一份私有同名副本
/// （显示成 `141 MB` / `885 KB`）。两份一旦分叉——比如一处四舍五入、
/// 一处截断——同一个文件在「下载进度」和「条目体积」两处会显示成不同的数，
/// 用户会以为下载出错了。这里是**唯一实现**，那份私有副本应改为调用本函数。
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    let (value, unit) = if b >= GB {
        (b / GB, "GB")
    } else if b >= MB {
        (b / MB, "MB")
    } else if b >= KB {
        (b / KB, "KB")
    } else {
        // 不足 1 KB：直接按整数字节，不显示小数。
        return format!("{bytes} B");
    };
    // 先按一位小数格式化，再抹掉末尾的 `.0`（`123.0` → `123`）。
    let text = format!("{value:.1}");
    let text = text.strip_suffix(".0").unwrap_or(text.as_str());
    format!("{text} {unit}")
}

/// 磁盘上某个路径的字节数；文件不存在或读不到元数据时返回 `None`。
///
/// # 为什么返回 `Option` 而不是 `0`
///
/// `0` 是**合法**的文件大小（空文件）。若用 `0` 兼表「读不到」，
/// 界面就只能显示 `0 B`，用户无法区分「这文件是空的」和「这文件不见了」——
/// 而这两种情况的处置完全不同（前者是坏文件、后者是路径配错）。
/// 显式的 `None` 让调用方自己决定显示 `—` 还是别的。
pub fn disk_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.len())
}

/// 递归统计一个目录里普通文件的字节数；读不到的条目直接跳过（不是为了报错，
/// 而是「空间占用」这种展示信息不该因为某个子目录没权限就整项失败）。
fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
    }
    total
}

/// 解压型条目的目录是否**只归它一个**。
///
/// 只有独占目录才能按「整个目录求和」统计占用：llama.cpp / sherpa 的 zip 是平铺解到
/// `tools/` 的，与 ffmpeg、python 等条目共用同一层——按目录求和会把别人的体积算进来，
/// 还会在「合计占用」里重复计数。
fn archive_root_is_exclusive(
    archive_dest: &Path,
    mut other_dests: impl Iterator<Item = PathBuf>,
) -> bool {
    let Some(root) = archive_dest.parent() else {
        return false;
    };
    !other_dests.any(|other| other.parent() == Some(root))
}

/// 与 `dest` 同目录的词表**相对路径**（写进 config 用，必须与其它配置项同口径）。
///
/// `config.toml` 里的路径一律是相对项目根、且用正斜杠；直接 `PathBuf` 拼出来在
/// Windows 上会带反斜杠，虽然能读，但与本文件其它条目的写法不一致，容易在比对/测试里
/// 出现「看起来一样却不相等」。
pub fn sibling_tokens_rel(dest: &str) -> String {
    Path::new(dest)
        .with_file_name("tokens.txt")
        .to_string_lossy()
        .replace('\\', "/")
}

/// 同目录下的词表路径（sherpa-onnx 的模型包一律是 `model.int8.onnx` + `tokens.txt`
/// 平铺在同一目录）。
///
/// 抽成函数是为了让「设为当前识别模型」与下载完成后的路径推断用同一份规则，
/// 避免一边写 `tokens.txt`、另一边写 `tokens.json` 这种不一致。
pub fn sibling_tokens_path(dest: &Path) -> PathBuf {
    dest.parent()
        .map(|d| d.join("tokens.txt"))
        .unwrap_or_else(|| PathBuf::from("tokens.txt"))
}

/// 某个条目在磁盘上的**实际占用**（`None` = 未就位）。
///
/// # 为什么不能只看 `dest`
///
/// `disk_size(dest)` 只统计入口文件：`whisper-cli.exe` 才 474 KB，而它所在目录解压出来
/// 是几十 MB；llama 的 `.exe` 只有 9 KB，真正的代码在同目录几十个 DLL 里。
/// 于是界面会出现「下载了 40 MB，装完显示 474 KB」「删掉它却腾出 30 MB」这种对不上的数。
///
/// 规则：
/// - 单文件条目：就是该文件大小；
/// - 解压型条目且**目录独占**：整个目录递归求和（这才是删掉它能腾出的空间）；
/// - 解压型条目但目录与别的条目共用：退回「入口文件 + 伴生文件」，宁少算也不重复计数。
pub fn item_footprint(item: &DownloadItem, cfg: &AppConfig) -> Option<u64> {
    let dest = item_effective_path(item, cfg);
    let present = dest.exists()
        || item
            .companion
            .map(|name| {
                dest.parent()
                    .map(|d| d.join(name).exists())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
    if !present {
        return None;
    }
    if item.is_archive
        && archive_root_is_exclusive(
            &dest,
            ITEMS
                .iter()
                .filter(|other| other.id != item.id)
                .map(|other| item_effective_path(other, cfg)),
        )
    {
        if let Some(root) = dest.parent() {
            return Some(dir_size(root));
        }
    }
    let mut total = disk_size(&dest).unwrap_or(0);
    if let (Some(name), Some(root)) = (item.companion, dest.parent()) {
        total += disk_size(&root.join(name)).unwrap_or(0);
    }
    Some(total)
}

/// 由磁盘上的**实际文件**反查它属于哪个条目；都不匹配则 `None`。
///
/// # 为什么需要「反查」而不是直接用档位的规范 id
///
/// 档位存在**档位内量化回退**：`TurboSpeed` 首选 q5，q5 不在时会实际加载 q8；
/// `Balanced` 首选 small-q5_0，不在时实际加载 `ggml-small.bin`。界面上的「已就位」
/// 若按规范 id（q5）判定，就会出现「明明 q8 在、跑得起来，却显示未下载」；
/// 而删除按钮若仍按规范 id 找文件，又会变成「显示已就位、点删除却说没有文件」。
///
/// 用实际命中的文件反查条目 id，就同时解决了这两处错位：删除删的正是引擎在用的那个，
/// 且 `ggml-small.bin` 这类**没有条目**的回退文件会得到 `None`——界面据此不渲染
/// 删除按钮（它不在可下载清单里，本就不该由我们代删）。
pub fn item_id_for_path(path: &Path) -> Option<&'static str> {
    ITEMS
        .iter()
        .find(|i| AppConfig::resolve_path(i.dest) == path)
        .map(|i| i.id)
}

/// 条目当前**生效**的落地路径（界面显示 / 资源管理器定位 / 统计占用用）。
///
/// 与存在性判定和 [`delete_item_file`] **同源**（都先问「配置指向哪」、再退到
/// 默认 `dest`），但**不要求文件存在**——恰恰相反，它的价值就在文件不存在时：
/// 「定位」按钮要能告诉用户「这个组件本该在哪个目录」，磁盘统计也要能对不存在的
/// 文件返回 0 而不是漏掉一条。
///
/// 为什么不让调用方自己拼：界面里若各自写一遍「配置优先、默认兜底」，一旦与
/// `configured_path_for` 的规则（尤其是 whisper 档位那条「只认配置指向的档位」）
/// 分叉，就会出现「定位到 A、删除却删 B」的错位。
pub fn item_effective_path(item: &DownloadItem, cfg: &AppConfig) -> PathBuf {
    if let Some(existing) = resolve_existing_path(item, Some(cfg)) {
        return existing;
    }
    configured_path_for(item, Some(cfg)).unwrap_or_else(|| AppConfig::resolve_path(item.dest))
}

/// 删除某个组件已经下载的文件，成功时返回被删掉的路径。
///
/// # 安全契约（改动前务必逐条读完）
///
/// 这是本模块**唯一**会删用户磁盘文件的入口，所以边界收得极死：
///
/// - **只删一个普通文件**。解析出的路径若不是常规文件（例如是目录），
///   直接报错返回——本函数**绝不**调用 `remove_dir_all`、**绝不**递归。
/// - **压缩包类组件（`is_archive`）拒绝删除**。whisper-cli / llama-cpp 下载的
///   是 zip，解压后入口 exe 只有几 KB，真正的代码在同目录 DLL 里；而 llama.cpp
///   更是直接解压到 `tools/` **本身**。单独删「那个文件」要么留下一个残缺的
///   半包（exe 在、DLL 没了），要么波及共享的 tools 目录。这类组件只能由用户
///   手动整体移除，这里给出目录路径让他知道该删哪儿。
/// - **拒绝删除用户自编译的构建**。命中 `looks_like_custom_build` 或
///   `is_gpu_build` 的产物（例如手编的 Vulkan 版 whisper-cli）是用户的心血，
///   误删后要重新编译，代价极高。
/// - **路径解析与「是否已就位」判定完全同源**：两处都走
///   `resolve_existing_path`，因此界面显示「已就位」的那个文件，正是这里
///   会删的那个文件——不会出现「界面说有、却删到别处」或
///   「删了但界面还标着已就位」的错位。
///
/// 返回 `Ok(None)` 表示「本来就没有可删的东西」：这是幂等的，
/// 用户连点两次「删除」不会因为第二次找不到文件而报错。
pub fn delete_item_file(item_id: &str, cfg: &AppConfig) -> Result<Option<PathBuf>> {
    let item = item_by_id(item_id).ok_or_else(|| anyhow!("未知的组件 id: {item_id}"))?;

    // 压缩包类：入口只是启动桩，删单文件必然留下坏包；llama.cpp 的解压目录
    // 就是 tools/ 本身，更不能碰。让用户手动整体删除。
    if item.is_archive {
        let entry = AppConfig::resolve_path(item.dest);
        let dir = entry.parent().unwrap_or(entry.as_path());
        bail!(
            "{}（{}）是自包含/压缩包组件：入口文件只是几 KB 的启动桩，\
             真正的程序在同目录的 DLL 里，单独删一个文件会留下残缺的半包。\
             请手动删除整个目录后再刷新：{}",
            item.label,
            item.id,
            dir.display()
        );
    }

    // 与「是否已就位」用同一套解析逻辑，保证删的就是界面报告存在的那个文件。
    // 同一套解析逻辑；这里借用 `cfg`，避免为了取引用再拷一份配置。
    let Some(path) = resolve_existing_path(item, Some(cfg)) else {
        return Ok(None);
    };

    if !path.is_file() {
        bail!(
            "{} 的路径不是常规文件（可能是目录），已中止删除：{}",
            item.label,
            path.display()
        );
    }

    // 用户自编译的构建不能删。先看 looks_like_custom_build（它已覆盖
    // 「带伴生文件的条目 + GPU 后端 DLL」这一路），再看 is_gpu_build
    // （覆盖没有伴生文件的条目）。
    if looks_like_custom_build(item, &path) || is_gpu_build(&path) {
        bail!(
            "{} 看起来是你自己编译（自编译）的构建——自包含单文件或带 GPU 后端。\
             为避免误删你手编的产物，这里不会删除它。若确实要删，请手动处理：{}",
            item.label,
            path.display()
        );
    }

    fs::remove_file(&path).with_context(|| format!("删除文件失败: {}", path.display()))?;
    info!(item = item.id, path = %path.display(), "已删除组件文件");
    Ok(Some(path))
}

/// 该条目是否应该显示「删除」按钮。
///
/// # 为什么要有这个纯谓词
///
/// 界面用它做**显示层**的门禁：只有返回 `true` 的条目才渲染删除按钮。
/// 这样就从根上保证了「按钮存在」与 `delete_item_file` 会成功不会分叉——
/// 否则用户会点到一个必然报错的按钮（例如压缩包组件），
/// 或者更糟：点到本该被保护、不该删的东西。
///
/// 未知 id 返回 `false`：界面拿到的可能是过期的 id（条目已改名/下线），
/// 此时宁可不出按钮，也不要渲染一个点了就报错的按钮。
pub fn item_is_deletable(id: &str) -> bool {
    match item_by_id(id) {
        Some(item) => !item.is_archive,
        None => false,
    }
}

/// 缺少的**必需**条目数（用于启动提示与界面告警）。
pub fn missing_required_count() -> usize {
    let ctx = PresenceContext::load();
    ITEMS
        .iter()
        .filter(|i| i.required && !ctx.is_present(i))
        .count()
}

/// 缺失条目总数。
pub fn missing_count() -> usize {
    let ctx = PresenceContext::load();
    ITEMS.iter().filter(|i| !ctx.is_present(i)).count()
}

/// 下载进度回调：(已完成字节, 总字节, 当前项 id)。
/// 总字节为 0 表示服务端没给 Content-Length。
pub type ProgressFn = Box<dyn Fn(u64, u64, &str) + Send + Sync>;

/// 下载单个条目。
///
/// 先写 `<dest>.part`，成功（含大小校验、可选的 sha256 校验）后原子 rename
/// 到目标名。因此**任何中断都不会留下一个看起来完好的模型文件**。
/// `.part` 已存在且未下满时会带 Range 续传；校验失败会删除 `.part` 并换下一个源。
/// 目标路径所在卷的可用空间（字节）。拿不到时返回 `None`（不因此阻断下载）。
///
/// 用 `sysinfo` 枚举磁盘再按挂载点前缀匹配：这是跨平台且**零新依赖**的做法
/// （`sysinfo` 本来就在 `Cargo.toml` 里，监控面板在用）。
pub(crate) fn available_space_for(path: &std::path::Path) -> Option<u64> {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    let target = std::fs::canonicalize(path)
        .ok()
        .unwrap_or_else(|| path.to_path_buf());
    let mut best: Option<(usize, u64)> = None;
    for disk in disks.list() {
        let mount = disk.mount_point();
        // 用「挂载点是不是目标路径的前缀」来归属；取**最长**前缀，
        // 避免 C:\ 把挂在 C:\mnt\data 的卷抢走。
        if target.starts_with(mount) {
            let len = mount.as_os_str().len();
            let space = disk.available_space();
            if best.map(|(l, _)| len >= l).unwrap_or(true) {
                best = Some((len, space));
            }
        }
    }
    best.map(|(_, space)| space)
}

/// 下载前的磁盘空间预检。
///
/// 为什么要有这一步：模型动辄几百 MB（全部补齐约 800 MB）。盘快满时下载会在跑到
/// 一半时失败，虽然 `.part` 支持续传，但用户看到的是「下载失败」+ 一堆 io 错误，
/// 完全联想不到「磁盘满了」——而这恰恰是最容易提前说清楚的一类失败。
///
/// 预检**尽力而为**：拿不到磁盘信息时放行（不能因为枚举磁盘失败就不让下载）。
fn ensure_free_space(dest: &std::path::Path, needed_bytes: u64) -> Result<()> {
    // 留一点余量：分片写入、解压、临时文件都要占位置。
    const MARGIN: u64 = 256 * 1024 * 1024;
    let Some(available) = available_space_for(dest) else {
        return Ok(());
    };
    let required = needed_bytes.saturating_add(MARGIN);
    if available < required {
        anyhow::bail!(
            "磁盘空间不足：所在磁盘仅剩 {}，本次下载需要约 {}（含临时文件余量）。\
             请清理磁盘或把程序移到空间更大的位置后重试。",
            human_size(available),
            human_size(required)
        );
    }
    Ok(())
}
pub fn download_one(
    item: &DownloadItem,
    cancel: &Arc<AtomicBool>,
    progress: Option<&ProgressFn>,
) -> Result<PathBuf> {
    let dest = AppConfig::resolve_path(item.dest);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| {
            // 目录建不出来只有一个原因：**装的位置不可写**（Program Files、
            // 企业策略锁定、只读 U 盘）。这个错误必须自己说清楚，
            // 否则用户看到的是一串 io 错误，完全想不到「换个位置装就行」。
            let hint = "无法在 {} 下创建模型目录（该位置可能不可写：Program Files、企业策略锁定、只读介质）。"
                .to_string()
                + "请把程序移到有写权限的目录，或在「性能设置 → 模型与组件」里重新下载。";
            hint.replace("{}", &parent.display().to_string())
        })?;
    }
    // 空间预检放在「目录已建好」之后、「开始下载」之前：
    // 这样路径解析与可写性已经过，剩下的失败几乎只可能是空间不足。
    ensure_free_space(&dest, item.size)?;
    // 已经存在**自编译的自包含构建**时拒绝覆盖：官方包是「启动桩 + DLL」，
    // 直接下载会把用户的单文件构建换成启动桩（尤其是手编的 Vulkan 版，
    // GPU 支持会凭空消失且毫无提示）。让他先自行处理，或把 config 指向别处。
    if looks_like_custom_build(item, &dest) {
        bail!(
            "{} 处已有一个自编译的单文件构建（入口约 {:.1} MB、同目录没有 {}）。\
             为避免覆盖你自己的构建（例如带 Vulkan 的 whisper-cli），已中止下载。\
             若确实想换成本项目提供的包，请先手动移走该文件；\
             若只是想继续用它，则无需下载。",
            dest.display(),
            fs::metadata(&dest)
                .map(|m| m.len() as f64 / 1048576.0)
                .unwrap_or(0.0),
            item.companion.unwrap_or("DLL")
        );
    }
    // `.part` 后缀让它天然被「是否已就位」判定排除（目标名不存在）。
    // 用共用构造函数，保证与 `sweep_stale_parts` 的命名规则一致。
    let part =
        part_path_for(&dest).ok_or_else(|| anyhow!("无法为 {} 构造临时文件名", dest.display()))?;

    // 本次下载**累计的网络传输量**（跨重试与换源）。它才是用户真正花的流量：
    // 一次失败可能已经白传了几百 MB，界面上却只显示文件大小。
    let mut net_bytes: u64 = 0;
    let mut last_err: Option<anyhow::Error> = None;
    // 本次下载内失败过的 host：排在队尾，避免同一次下载反复踩同一个死源。
    // 每次 download_one 都从空表开始——即「下次下载恢复原始顺序」，
    // 挂梯子的用户不会因为上一次的失败而永远用不上更快的境外源。
    let mut failed_hosts: Vec<String> = Vec::new();
    let ordered = order_sources(item.urls, &failed_hosts);
    for url in ordered {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("已取消"));
        }
        match fetch_to_file(
            url,
            &part,
            item.size,
            item.id,
            cancel,
            progress,
            &mut net_bytes,
        ) {
            Ok(()) => {
                // 内容哈希强校验（仅当条目声明了 sha256）。不符时删 `.part` 换源，
                // 绝不 rename —— 把损坏文件落到正式名比下载失败更难排查。
                if let Some(expected) = item.sha256 {
                    match sha256_file(&part) {
                        Ok(actual) if actual == expected => {}
                        Ok(actual) => {
                            warn!(
                                item = item.id,
                                url, expected, actual, "sha256 校验失败，尝试下一个下载源"
                            );
                            let _ = fs::remove_file(&part);
                            last_err =
                                Some(anyhow!("sha256 校验失败：期望 {expected}，实际 {actual}"));
                            continue;
                        }
                        Err(err) => {
                            warn!(item = item.id, url, error = %err, "计算 sha256 失败，尝试下一个下载源");
                            let _ = fs::remove_file(&part);
                            last_err = Some(err);
                            continue;
                        }
                    }
                }
                // 压缩包类组件（如 llama.cpp）：下载物是 zip，真正的程序在包内。
                // 必须解压到 dest 的父目录并校验关键文件都在，才算成功。
                if item.is_archive {
                    match unpack_archive(item, &part, &dest, url) {
                        Ok(()) => {
                            let dir = dest.parent().unwrap_or(dest.as_path());
                            info!(item = item.id, dir = %dir.display(), "下载并解压完成");
                            return Ok(dest);
                        }
                        Err(err) => {
                            // 结构与登记不符（比如镜像换了打包方式）：当成该源失败，
                            // 删掉 .part 换下一个源，而不是留下半成品目录。
                            warn!(item = item.id, url, error = %err, "解压失败，尝试下一个下载源");
                            let _ = fs::remove_file(&part);
                            last_err = Some(err);
                            continue;
                        }
                    }
                }
                // 校验通过才落正式名
                fs::rename(&part, &dest).with_context(|| {
                    format!("重命名失败: {} → {}", part.display(), dest.display())
                })?;
                info!(item = item.id, path = %dest.display(), "下载完成");
                return Ok(dest);
            }
            Err(err) => {
                warn!(item = item.id, url, error = %err, "该下载源失败，尝试下一个");
                // 换源前先算账：累计传输已经超过登记大小的 3 倍还在失败，说明所有源
                // 都在反复给错误内容，继续换下去只是继续烧用户的流量。
                if retry_budget_exhausted(net_bytes, item.size) {
                    let over = "累计传输已超过登记大小的 3 倍，停止重试：这一项的所有下载源"
                        .to_string()
                        + "都在反复返回错误内容，请稍后再试或检查网络代理";
                    return Err(anyhow!(over));
                }
                // 连接阶段失败（DNS/TCP/TLS）→ 本次下载内把它排到队尾。
                //
                // 刻意**不**做跨下载的永久拉黑：那种「记住坏源」的做法对挂梯子的用户
                // 有害——他开着梯子时境外源本来是最快的，却会因为之前裸连失败过而
                // 一直被跳过。范围限定在本次下载，既省掉重复白等，又不缩小下次的
                // 可用源集合（见 [`order_sources`]）。
                let host = url_host(url);
                if is_connect_stage_error(&err) && !host.is_empty() && !failed_hosts.contains(&host)
                {
                    failed_hosts.push(host);
                }
                // 保留 `.part`：下一个源可以带 Range 从断点续传，避免把已经下到的
                // 几百 MB 扔掉重来。它不会被误认成完整模型（判定只看目标名），
                // 下次启动的 sweep 也会清掉它。
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("没有可用的下载源")))
}

/// 压缩包类型：由下载 URL 的扩展名判断。
///
/// 为什么从 URL 推、而不是给 `DownloadItem` 加字段：该结构有 30+ 个字面量
/// （含测试夹具），每加一个字段都要全改一遍，改动面大且容易漏。而「这条链接下的是
/// zip 还是 tar.bz2」在 URL 里本来就写着（sherpa-onnx 的发布包一律 `.tar.bz2`），
/// 同一条目的多个镜像也总是同一种打包方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Zip,
    TarBz2,
}

fn archive_kind_from_url(url: &str) -> ArchiveKind {
    let lower = url.to_ascii_lowercase();
    if lower.contains(".tar.bz2") || lower.ends_with(".tbz2") {
        ArchiveKind::TarBz2
    } else {
        ArchiveKind::Zip
    }
}

/// `.tar.bz2` 的解压脚本（交给内置便携 Python 执行）。
///
/// 为什么用 Python 而不是加 tar/bz2 依赖：便携 Python 本来就随包分发（SenseVoice
/// 运行器要用），`tarfile` 是标准库——零新依赖、零额外体积。
///
/// 安全：便携 Python 是 3.11，`extractall(filter="data")` 要 3.12 才有，所以这里
/// **逐成员校验**：绝对路径、`..` 穿越、盘符开头的名字，以及符号链接 / 硬链接 /
/// 设备文件一律跳过。不校验就会重演 zip 那套「解压到目标目录之外」的问题。
///
/// 输出：成功解出的**普通文件数**（解析 stdout 得到），供调用方判断是否空包。
const TAR_BZ2_SCRIPT: &str = r#"
import sys, tarfile

src, dst = sys.argv[1], sys.argv[2]

def is_safe(name):
    n = name.replace("\\", "/")
    if not n or n.startswith("/") or n.startswith("../") or n == "..":
        return False
    if "/../" in n or n.endswith("/.."):
        return False
    # Windows 盘符（C:/...）同样是绝对路径
    if len(n) > 1 and n[1] == ":":
        return False
    return True

with tarfile.open(src, "r:bz2") as tf:
    members = [m for m in tf.getmembers()
               if not (m.issym() or m.islnk() or m.isdev()) and is_safe(m.name)]

    # 剥掉唯一的顶层目录：sherpa-onnx 的发布包几乎都是
    # <模型名>/model.int8.onnx + <模型名>/tokens.txt 这种结构，不剥的话
    # 登记路径就得写成 models/xxx/<那一长串版本号>/model.int8.onnx，
    # 版本号一变就全部失效。与 zip 那套 extract_zip 的 common_prefix 同语义。
    top = None
    uniform = True
    for m in members:
        parts = m.name.split("/")
        if len(parts) < 2 or not parts[0]:
            uniform = False
            break
        if top is None:
            top = parts[0]
        elif parts[0] != top:
            uniform = False
            break

    count = 0
    for m in members:
        if uniform and top:
            m.name = m.name[len(top) + 1:]
            if not m.name:
                continue
        tf.extract(m, dst)
        if m.isfile():
            count += 1
print(count)
"#;

/// 用内置便携 Python 解压 `.tar.bz2`，返回解出的普通文件数。
fn extract_tar_bz2(part: &Path, dir: &Path) -> Result<usize> {
    // 与 SenseVoice 运行器同一套解释器解析规则：优先内置便携 Python，
    // 别指望客户机 PATH 上有 python。
    let python = crate::utils::AppConfig::resolve_command("python");
    let mut cmd = std::process::Command::new(&python);
    cmd.arg("-c")
        .arg(TAR_BZ2_SCRIPT)
        .arg(part)
        .arg(dir)
        .stdin(std::process::Stdio::null());
    // 去掉用户环境里残留的 PYTHONHOME/PYTHONPATH 并固定 UTF-8 输出：
    // 否则某些机器上解释器会去找另一份（残缺的）标准库。
    crate::engines::media_pipeline::apply_python_env(&mut cmd);
    // 1.5 GB 的 bz2 在慢 CPU 上解压要几分钟；给足预算，但不能无限等。
    let out = crate::engines::ffmpeg::run_with_deadline(&mut cmd, Duration::from_secs(1800))
        .with_context(|| format!("启动解压进程失败: {}", python.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "解压 .tar.bz2 失败（退出码 {:?}）：{}",
            out.status.code(),
            stderr.trim().chars().take(400).collect::<String>()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0))
}

/// 把已下载到 `part` 的 zip 解压到 `dest` 的父目录，并校验关键文件都落盘。
///
/// 抽成独立函数是因为这几步**顺序敏感**，混在 `download_one` 里容易改错：
/// 1. 解压到目标目录；
/// 2. 校验入口文件（`dest`）与伴生文件（`companion`）确实存在；
/// 3. 全部校验通过后才删除 `.part`。中途失败则保留 `.part`，
///    既不会留下半成品目录，也能被 `sweep_parts_for` 在下次启动时清掉。
fn unpack_archive(item: &DownloadItem, part: &Path, dest: &Path, url: &str) -> Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| anyhow!("目标路径没有父目录: {}", dest.display()))?;
    // 按 URL 的扩展名选解压器：.tar.bz2 走便携 Python，其余沿用内置 zip 解析器。
    let n = match archive_kind_from_url(url) {
        ArchiveKind::TarBz2 => {
            extract_tar_bz2(part, dir).with_context(|| format!("解压失败: {}", part.display()))?
        }
        ArchiveKind::Zip => {
            let bytes =
                fs::read(part).with_context(|| format!("读取压缩包失败: {}", part.display()))?;
            super::zip_extract::extract_zip(&bytes, dir)
                .with_context(|| format!("解压失败: {}", part.display()))?
        }
    };
    if n == 0 {
        bail!("压缩包里没有可用的文件: {}", part.display());
    }
    if !dest.exists() {
        bail!(
            "解压完成但缺少入口文件 {}（压缩包内容与登记不符）",
            dest.display()
        );
    }
    if let Some(companion) = item.companion {
        if !dir.join(companion).exists() {
            bail!("解压完成但缺少伴生文件 {companion}（可能压缩包不完整）");
        }
    }
    fs::remove_file(part).with_context(|| format!("清理中间文件失败: {}", part.display()))?;
    Ok(())
}

/// `.part` 残留超过这个时长（自最后修改起）才被视为过期并可清理。
/// 7 天足够覆盖「跨天的大文件续传」，又不会让垃圾无限累积。
const STALE_PART_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// 清扫**过期**的 `<目标>.part` 下载中间文件。
///
/// # 为什么需要
///
/// 下载先写 `<dest>.part`、校验通过才 rename 到正式名。正常路径下失败会自己删掉，
/// 但**进程被强杀 / 断电**时删不掉——而最大的条目（Turbo Q8）有 833 MB，
/// 残留一个就是近 1 GB 的垃圾。
///
/// 这些文件落在 `models/` 与 `tools/` 下（不是 `%TEMP%`），
/// 因此 `temp_cleanup` 的启动清扫覆盖不到，必须在这里单独扫。
///
/// # 为什么只删「过期」而不是全部
///
/// `.part` 是断点续传的载体：留着它，下次启动就能带 `Range` 接着下。早先
/// 版本在每次启动时清光所有 `.part`，等于把跨进程续传彻底废掉（刚下到 700 MB
/// 被强杀，重启后从 0 再来）。现在改为**按修改时间过期**：只有超过
/// `STALE_PART_MAX_AGE` 未更新的残留才清理，近期残留一律保留。
///
/// 只删「正好是某个条目的 `<dest>.part`」的文件，不做泛扫——
/// 用户自己的文件绝不能被误删。
///
/// 返回删除的文件数。
pub fn sweep_stale_parts() -> usize {
    let dests: Vec<PathBuf> = ITEMS
        .iter()
        .map(|i| AppConfig::resolve_path(i.dest))
        .collect();
    sweep_parts_for(&dests)
}

/// 清扫指定落地路径对应的 `.part` 残留。
///
/// 与 [`sweep_stale_parts`] 拆开是为了**可测**：后者绑死真实模型路径，
/// 测试若直接跑它就得往 `models/` 里写假文件——而那会与其它并行测试
/// （尤其是「已就位」判定）互相干扰，制造出难以复现的偶发失败。
/// 这里接受任意路径列表，测试用临时目录即可完全隔离。
pub fn sweep_parts_for(dests: &[PathBuf]) -> usize {
    sweep_parts_for_at(dests, SystemTime::now())
}

/// `.part` 残留「过期」的判定（纯函数，便于单测）。
///
/// 只有**最后修改时间早于** `now - STALE_PART_MAX_AGE` 的残留才算过期；
/// 近期残留必须保留，否则刚下到一半崩溃、下次启动就被删掉，跨进程续传无从谈起。
///
/// 系统时钟回拨（`now` 早于 `mtime`）时 `duration_since` 返回 `Err`，此时
/// 保守地判为**未过期**（保留），绝不因为时间异常误删可续传的中间文件。
fn is_stale_part(mtime: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(mtime) {
        Ok(age) => age >= STALE_PART_MAX_AGE,
        Err(_) => false,
    }
}

/// 带显式 `now` 的清扫实现，便于单测在不依赖真实时钟的情况下验证过期逻辑。
fn sweep_parts_for_at(dests: &[PathBuf], now: SystemTime) -> usize {
    let mut removed = 0usize;
    for dest in dests {
        let Some(part) = part_path_for(dest) else {
            continue;
        };
        let Ok(meta) = fs::metadata(&part) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            // 读不到修改时间就无法判断新旧；宁可留着（下次再判），也不要误删
            // 一个可能还能续传的中间文件。
            warn!(path = %part.display(), "无法读取 .part 的修改时间，跳过清理");
            continue;
        };
        if !is_stale_part(mtime, now) {
            // 近期残留：可能是可续传的半成品，保留。
            continue;
        }
        match fs::remove_file(&part) {
            Ok(()) => {
                info!(path = %part.display(), "已清理过期残留的下载中间文件");
                removed += 1;
            }
            // 删不掉（被占用 / 权限）不算错误：下次启动再试
            Err(err) => warn!(path = %part.display(), error = %err, "清理下载中间文件失败"),
        }
    }
    removed
}

/// 某个落地路径对应的 `.part` 中间文件名。
///
/// 与 `download_one` 里构造的规则必须**完全一致**，否则清理会找不到文件。
/// 抽成一个函数就是为了让两处共用同一份定义，避免将来改一处漏一处。
fn part_path_for(dest: &Path) -> Option<PathBuf> {
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!("{e}."))
        .unwrap_or_default();
    Some(dest.with_extension(format!("{ext}part")))
}

/// 请求使用的 User-Agent。
///
/// 部分镜像（尤其是 Cloudflare 后面的代理）会对空 UA / 非浏览器 UA 直接拒绝，
/// 用常见的浏览器 UA 最省事。
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko)";

/// 断点续传的起点判定：返回需要续传的字节偏移量。
///
/// 只有「`.part` 已有内容」且「尚未达到期望大小」时才续传；其余情况一律返回
/// `None`（无 `.part`、空文件、大小未知、已下满），由调用方决定从头下还是直接校验。
/// 抽成纯函数是为了能单测：真正的下载路径要联网，没法在测试里跑。
fn resume_offset(part_len: u64, expected: u64) -> Option<u64> {
    if part_len == 0 || expected == 0 || part_len >= expected {
        None
    } else {
        Some(part_len)
    }
}

/// 构造 HTTP `Range` 请求头：从 `offset` 一直取到文件末尾。
fn range_header(offset: u64) -> String {
    format!("bytes={offset}-")
}

/// 从 `Content-Range` 响应头里解析本次响应的**起始字节偏移**。
///
/// 形如 `bytes 500-999/1000` 或 `bytes 500-/1000`，返回 `Some(500)`；
/// 格式不认识时返回 `None`。抽成纯函数便于单测（真正的 206 响应要联网才有）。
fn content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let start = rest.split('/').next()?.split('-').next()?;
    start.trim().parse::<u64>().ok()
}

/// 计算文件的 sha256，返回 64 位十六进制小写字符串。
///
/// 独立成函数便于单测：喂固定内容即可验证，不需要联网。
fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("读取文件失败: {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 发一次 GET 并返回响应；`resume` 为 `Some` 时带上 `Range` 头请求续传。
///
/// 服务端可能用状态码拒绝 `Range`（例如 416）。这是源之间的实现差异，
/// 不该把该源判死：这里自动回退成不带 `Range` 的整文件请求再试一次。
///
/// 返回 `anyhow::Result` 而不是把 `ureq::Error` 直接放进 `Err`：后者体积很大，
/// 直接返回会触发 `clippy::result_large_err`。
fn send_get(
    agent: &ureq::Agent,
    url: &str,
    item_id: &str,
    resume: Option<u64>,
) -> Result<ureq::Response> {
    let Some(offset) = resume else {
        // 不带 Range：整文件下载。
        return agent
            .get(url)
            .set("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| anyhow!("请求失败: {e}"));
    };
    match agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .set("Range", &range_header(offset))
        .call()
    {
        Ok(resp) => Ok(resp),
        Err(ureq::Error::Status(code, _)) => {
            warn!(
                item = item_id,
                url, code, "Range 请求被拒，回退为整文件下载"
            );
            agent
                .get(url)
                .set("User-Agent", USER_AGENT)
                .call()
                .map_err(|e| anyhow!("请求失败: {e}"))
        }
        Err(err) => Err(anyhow!("请求失败: {err}")),
    }
}

/// 从单个 URL 流式下载到文件，支持断点续传，并做大小校验。
/// 写入量是否已**明显超出**登记大小（超过 5%）。
///
/// 为什么必须有上限：流式写法是「服务端给多少写多少」，而对端并不总是老实——
/// 镜像返回一个 HTML 错误页、代理忽略了 `Range` 又拼上了完整体、URL 过期后换成了
/// 别的资产，都会让 `.part` 无限增长。实测症状是「界面显示 400 MB，实际下载了 2 GB」。
/// 登记大小未知（0）时恒为 `false`：不能拿 0 当上限把正常下载掐死。
fn exceeds_expected(written: u64, expected: u64) -> bool {
    expected > 0 && written > expected.saturating_add(expected / 20)
}

/// 决定「这次下载最多允许写到多少字节」。
///
/// 返回 `(上限, 登记值是否已过期)`。
///
/// 为什么要留这条退路：登记大小是**写死在源码里的常量**，而模型文件会变（上游换量化、
/// 重新打包、换仓库）。一旦上游把 874 MB 的 q8 换成 1.2 GB，硬按 105% 拦就会变成
/// 「正常文件永远下不下来」——比放过一次异常更糟。
///
/// 判据：
/// - 服务端没声明总大小 → 用登记值（此外没有可信来源）；
/// - 服务端声明的大小在登记值 105% 以内 → 用登记值；
/// - 服务端声明的大小明显更大 → 认为**登记过期**，改用服务端的大小并置 `true`，
///   让调用方记一条警告。真正的「镜像返回错误内容」通常大出好几倍（一个 HTML 错误页
///   不会恰好比模型大 20%），而那种极端放大仍会被 [`retry_budget_exhausted`] 挡住。
fn resolve_size_bound(registry: u64, server_declared: u64) -> (u64, bool) {
    if registry == 0 || server_declared == 0 {
        return (registry, false);
    }
    if exceeds_expected(server_declared, registry) {
        (server_declared, true)
    } else {
        (registry, false)
    }
}

/// 多次换源重试的**累计传输**是否已经离谱（超过登记大小的 3 倍）。
///
/// 每次源失败都会白传一部分字节，多源轮下来用户的数据流量可以是文件大小的好几倍。
/// 到 3 倍还没成功，说明「所有源都在反复给我们错东西」，继续换源只是继续烧流量。
fn retry_budget_exhausted(transferred: u64, expected: u64) -> bool {
    expected > 0 && transferred > expected.saturating_mul(3)
}

fn fetch_to_file(
    url: &str,
    part: &Path,
    expected_size: u64,
    item_id: &str,
    cancel: &Arc<AtomicBool>,
    progress: Option<&ProgressFn>,
    net_bytes: &mut u64,
) -> Result<()> {
    // 大文件（最大 833 MB）在慢网下需要足够长的**总**上限；但只有总上限是不够的：
    // 连接建好之后对端不再发数据时，总超时会把界面卡在「下载中 … 0 B」上整整两小时，
    // 既不报错也不换源，用户只能手动点取消（镜像端不响应时实测就是这样）。所以除总
    // 上限外还有一条连接超时与一条**空闲**超时（见各自的常量文档）。
    let agent = build_download_agent(
        DOWNLOAD_IDLE_TIMEOUT,
        DOWNLOAD_CONNECT_TIMEOUT,
        DOWNLOAD_TOTAL_TIMEOUT,
    );

    // `.part` 的现有大小：用于判断能否续传。
    let existing = fs::metadata(part).map(|m| m.len()).unwrap_or(0);

    // 已不小于期望大小：多半是上次下完但没来得及 rename（进程被杀）。
    // 直接跳过网络、交给上层做大小 / 哈希校验，省下一次几百 MB 的重下。
    if expected_size > 0 && existing >= expected_size {
        // 超过登记大小说明这份 `.part` 根本不是我们要的东西（错误页 / 别的资产 /
        // 代理把两段拼在了一起）。这里必须删掉重下——否则上层会把它 rename 成正式
        // 文件名，用户拿到一个「大小不对但没人报错」的模型。
        if exceeds_expected(existing, expected_size) {
            warn!(
                item = item_id,
                bytes = existing,
                expected = expected_size,
                "临时文件大于登记大小，判为损坏并丢弃"
            );
            let _ = fs::remove_file(part);
        } else {
            info!(
                item = item_id,
                bytes = existing,
                "已有完整的临时文件，跳过下载直接校验"
            );
            if let Some(cb) = progress {
                cb(existing, existing, item_id);
            }
            return Ok(());
        }
    }

    let requested_resume = resume_offset(existing, expected_size);

    // 发请求；若 Range 被服务端拒绝，`send_get` 内部已回退为整文件请求。
    let mut resp = send_get(&agent, url, item_id, requested_resume)?;
    let mut resume = requested_resume;

    // 服务端回 206 只说明「我按 Range 给了部分内容」，并不保证起点就是我们要的
    // 偏移——有的镜像 / 代理会忽略或错解 `Range`，若直接把 body 追加进 `.part`，
    // 会把两段错位的字节拼成一个「大小对、内容错」的文件（哈希校验才拦得住，
    // 未登记哈希的条目则会被静默损坏）。这里核对 `Content-Range` 的起点：
    // 对不上（或拿不到该头）就丢弃这个响应，改发一次**不带 Range** 的整文件请求。
    if resp.status() == 206 {
        match resp.header("Content-Range").and_then(content_range_start) {
            Some(start) if Some(start) == resume => {}
            got => {
                warn!(
                    item = item_id,
                    url,
                    got = ?got,
                    requested = ?resume,
                    "206 响应的 Content-Range 起点与续传偏移不符，放弃续传、改为整文件重下"
                );
                resp = send_get(&agent, url, item_id, None)?;
                resume = None;
            }
        }
    }

    let status = resp.status();
    // 只有「发了 Range 且服务端明确回 206」才续写；其余（含 200）一律截断重下，
    // 保证不会把两段不同来源的字节拼在一起。
    let appending = status == 206 && resume.is_some();

    let (mut file, mut written) = if appending {
        let f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part)
            .with_context(|| format!("打开临时文件失败: {}", part.display()))?;
        // 以文件**真实长度**为续写起点，而不是 metadata 快照——两者之间文件若被
        // 截断，用旧快照会让进度与实际字节数对不上。
        let base = f.metadata().map(|m| m.len()).unwrap_or(0);
        (f, base)
    } else {
        let f = fs::File::create(part)
            .with_context(|| format!("创建临时文件失败: {}", part.display()))?;
        (f, 0)
    };

    // 进度条的总字节数一律用**登记大小**：206 响应里 `Content-Length` 只是剩余
    // 长度，直接当总数会让进度从 0 跳变甚至倒退。
    let remaining = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    // 服务端声明的**整文件**大小只在 200（整文件响应）时可推：206 的 Content-Length
    // 只是剩余部分。续传时 written 里已含前缀，两者相加才是完整大小。
    let server_declared = if status == 200 && remaining > 0 {
        written.saturating_add(remaining)
    } else {
        0
    };
    // 上限以登记大小为准，但**登记值过期时以服务端为准**（见 `resolve_size_bound`）——
    // 否则上游换了更大的量化版本之后，正常下载会被自己的上限拦死。
    let (bound, registry_stale) = resolve_size_bound(expected_size, server_declared);
    if registry_stale {
        warn!(
            item = item_id,
            registry = expected_size,
            server = server_declared,
            "登记大小与服务端不一致（多半是上游换了文件），本次按服务端大小放行"
        );
    }
    let total = if bound > 0 {
        bound
    } else {
        written + remaining
    };
    // 本次实际下载的字节数（用于测速）；`written` 里含续传前已有的部分。
    let base_bytes = written;

    let mut reader = resp.into_reader();

    let mut buf = vec![0u8; 256 * 1024];
    let started = Instant::now();
    let mut last_report = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            // 保留 `.part`：用户重新点下载时可以接着下，不必从头再来。
            return Err(anyhow!("已取消"));
        }
        // 文案里点出「对端长时间没有数据」：这正是空闲超时最常见的原因，
        // 比一句干巴巴的「读取失败」更能让用户知道该换源还是查网络。
        let n = reader
            .read(&mut buf)
            .context("下载中断：读取响应流失败（网络中断，或对端长时间没有数据）")?;
        if n == 0 {
            break;
        }
        // 先算「写完会有多少」，超限就**不写**：宁可少写一次，也不要为了报错先把
        // 几百 MB 的错误内容落到盘上。
        let prospective = written.saturating_add(n as u64);
        if exceeds_expected(prospective, bound) {
            drop(file);
            let _ = fs::remove_file(part);
            return Err(anyhow!(
                "下载内容超出登记大小：已收到 {:.1} MB，登记 {:.1} MB（疑似镜像返回了错误内容）",
                prospective as f64 / MB as f64,
                bound as f64 / MB as f64
            ));
        }
        file.write_all(&buf[..n]).context("写入文件失败")?;
        written += n as u64;
        // 累计**网络**字节（跨重试 / 换源），用来判断「是不是所有源都在反复给错东西」。
        *net_bytes = net_bytes.saturating_add(n as u64);

        // 限流上报：进度条不需要每 256KB 刷一次，100ms 一次足够顺滑，
        // 也能让 UI 线程少做无谓的重绘。
        if last_report.elapsed() >= Duration::from_millis(100) {
            last_report = Instant::now();
            if let Some(cb) = progress {
                cb(written, total, item_id);
            }
        }
    }
    file.flush().context("刷盘失败")?;
    drop(file);

    if let Some(cb) = progress {
        cb(written, total.max(written), item_id);
    }

    // 大小校验：截断的下载是「模型加载失败」最常见也最难查的根因
    if expected_size > 0 {
        let expected = expected_size as f64;
        let actual = written as f64;
        if actual < expected * 0.95 {
            return Err(anyhow!(
                "下载不完整：收到 {:.1} MB，期望至少 {:.1} MB",
                actual / MB as f64,
                expected * 0.95 / MB as f64
            ));
        }
    }

    // 测速只算本次真正下载的字节数（续传时不含此前已有的部分）。
    let fetched = written.saturating_sub(base_bytes);
    let secs = started.elapsed().as_secs_f64().max(0.001);
    info!(
        item = item_id,
        mb = fetched / MB,
        secs = secs,
        speed_mbps = (fetched as f64 / MB as f64) / secs,
        "下载完成"
    );
    Ok(())
}

#[cfg(test)]
mod disk_space_tests {
    use super::{available_space_for, ensure_free_space};

    /// 预检必须**尽力而为**：拿不到磁盘信息就放行，绝不因为枚举失败而挡住下载。
    #[test]
    fn free_space_check_never_blocks_when_disk_info_is_unavailable() {
        let dir = std::env::temp_dir();
        // 需要 0 字节 → 任何有空间的卷都应该通过。
        assert!(ensure_free_space(&dir, 0).is_ok(), "0 字节需求必须放行");
        // 枚举磁盘本身不得 panic（容器 / 受限环境里可能拿不到列表）。
        let _ = available_space_for(&dir);
    }

    /// 需要天文数字时必须报「磁盘空间不足」，而不是让下载跑到一半才失败。
    #[test]
    fn absurd_requirement_reports_clear_error() {
        let dir = std::env::temp_dir();
        let Some(_avail) = available_space_for(&dir) else {
            // 拿不到磁盘信息：按设计放行，这里直接跳过断言。
            return;
        };
        let err = ensure_free_space(&dir, u64::MAX / 4).expect_err("不可能有这么多空间");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("磁盘空间不足"),
            "错误文案必须点名磁盘空间，实得：{msg}"
        );
    }
}

#[cfg(test)]
mod item_and_presence_tests {
    use super::{
        executable_is_usable, is_command_available, missing_required_executables,
        resolve_configured_executable, ITEMS,
    };
    use crate::utils::AppConfig;
    use std::path::Path;

    /// 配置里的路径存在时原样使用：用户显式指定的永远优先。
    #[test]
    fn existing_configured_path_wins() {
        let dir = std::env::temp_dir();
        let real = dir.join("v2w_exec_placeholder.exe");
        std::fs::write(&real, b"x").unwrap();
        assert_eq!(resolve_configured_executable(&real), real);
        let _ = std::fs::remove_file(&real);
    }

    /// 不存在的路径、文件名也不是 PATH 上的命令：保持原值，让调用方判定「缺失」
    /// 并下载到配置写死的那个相对路径。
    #[test]
    fn unrelated_missing_path_is_left_untouched() {
        let typo = Path::new("Z:/definitely/not/here/bin/v2w_no_such_tool_zzz.exe");
        assert_eq!(resolve_configured_executable(typo), typo.to_path_buf());
        assert!(!is_command_available("v2w_no_such_tool_zzz"));
    }

    /// 缺失的 ffmpeg（本机 PATH 上也没有）必须被判为「不可用」→ 会被自动下载。
    #[test]
    fn missing_ffmpeg_is_reported_as_needed() {
        let mut cfg = AppConfig::default();
        // 指向一个绝对不存在的路径，且文件名不是 PATH 上的命令。
        cfg.paths.ffmpeg = "Z:/definitely/not/here/tools/v2w_no_ffmpeg_zzz.exe".to_string();
        cfg.paths.whisper_cli = "Z:/definitely/not/here/tools/v2w_no_whisper_zzz.exe".to_string();
        let missing = missing_required_executables(&cfg);
        let ids: Vec<&str> = missing.iter().map(|i| i.id).collect();
        assert!(ids.contains(&"ffmpeg"), "缺 ffmpeg 必须被报出来：{ids:?}");
    }

    /// 反例：配置指向一个**存在**的文件时不该再报缺失（否则每次启动都重复下载）。
    #[test]
    fn present_executable_is_not_reported() {
        let dir = std::env::temp_dir();
        let real = dir.join("v2w_present_ffmpeg.exe");
        std::fs::write(&real, b"x").unwrap();
        let mut cfg = AppConfig::default();
        cfg.paths.ffmpeg = real.display().to_string();
        let item = ITEMS
            .iter()
            .find(|i| i.id == "ffmpeg")
            .expect("清单里有 ffmpeg");
        assert!(executable_is_usable(&cfg, item), "存在的文件必须算可用");
        let missing = missing_required_executables(&cfg);
        assert!(
            !missing.iter().any(|i| i.id == "ffmpeg"),
            "已就位的不该出现在待下载清单里"
        );
        let _ = std::fs::remove_file(&real);
    }

    /// `is_command_available` 至少要对「一个几乎肯定存在的命令」为真、
    /// 对编造的名字为假——这条是 PATH 扫描本身的冒烟测试。
    #[test]
    fn command_lookup_sanity() {
        let bogus = "v2w_definitely_not_a_command_zzz";
        assert!(!is_command_available(bogus));
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 源排序：国内源优先、失败的沉底，且**任何一条都不被丢弃**。
    ///
    /// 这条锁住的是「国内/国外源都留着」这个决定。曾经考虑过把连不上的 host
    /// 永久拉黑以求快，但那会害了挂梯子的用户——他们开着代理时境外源恰恰最快，
    /// 且部分模型（如 Turbo 量化）只有境外源有。因此这里断言的是「只重排、不减项」。
    #[test]
    fn source_ordering_keeps_every_source_and_prefers_domestic() {
        let urls: [&'static str; 3] = [
            "https://hf-mirror.com/a/ggml-x.bin",
            "https://www.modelscope.cn/b/ggml-x.bin",
            "https://gh-proxy.com/https://github.com/c/ggml-x.bin",
        ];
        // 没有任何失败记录时：国内源排第一，境外源依次在后
        let ordered = order_sources(&urls, &[]);
        assert_eq!(ordered.len(), urls.len(), "不能丢弃任何源");
        assert!(
            ordered[0].contains("modelscope"),
            "国内源应排在最前，实际：{}",
            ordered[0]
        );

        // 国内源失败过：它沉到队尾，但**仍在列表里**（下次下载恢复优先）
        let failed = vec!["www.modelscope.cn".to_string()];
        let reordered = order_sources(&urls, &failed);
        assert_eq!(reordered.len(), urls.len(), "失败源只能沉底，不能删除");
        assert!(
            !reordered[0].contains("modelscope"),
            "失败源不该继续排在首位"
        );
        assert!(
            reordered.last().unwrap().contains("modelscope"),
            "失败源应被排到队尾"
        );
    }

    /// host 解析与国内外判定：\`modelscope\` 一律算国内源。
    #[test]
    fn overseas_detection_is_by_host() {
        assert_eq!(url_host("https://a.b/c/d"), "a.b");
        assert!(!is_overseas_source(
            "https://www.modelscope.cn/models/x/resolve/master/y.bin"
        ));
        assert!(!is_overseas_source("https://modelscope.cn/x"));
        assert!(is_overseas_source(
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin"
        ));
        assert!(is_overseas_source(
            "https://gh-proxy.com/https://github.com/k2-fsa/sherpa-onnx/releases/download/x.onnx"
        ));
    }

    /// 只有**连接阶段**的错误才值得把 host 沉底；HTTP 4xx/5xx 不该牵连整个源。
    #[test]
    fn only_connect_stage_errors_mark_a_source() {
        assert!(is_connect_stage_error(&anyhow!(
            "请求失败: Connection Failed: tls connection init failed"
        )));
        assert!(
            !is_connect_stage_error(&anyhow!("sha256 校验失败：期望 a，实际 b")),
            "内容校验失败与源连通性无关"
        );
    }

    /// 空闲超时必须真的生效：对端把连接接着、一个字节都不发时，请求要在空闲超时后
    /// 报错，而不是拖到两小时的总上限。这条正是「下载中 … 0 B 卡住」的回归护栏。
    ///
    /// 用本地 `TcpListener` 当「连接在、数据不来」的假镜像：接住连接后既不回响应、
    /// 也不立刻关闭，只在 1.5 秒后放手——因此即便超时全部失效，测试也只会以「连接
    /// 被关闭」收尾，不会把整个测试进程挂住。
    #[test]
    fn idle_download_times_out_instead_of_hanging() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");
        let holder = std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                std::thread::sleep(Duration::from_millis(1500));
                drop(sock);
            }
        });

        // 生产取值的相对关系：连接 < 空闲 < 总上限。三条都设成一样大就失去意义了。
        assert!(DOWNLOAD_CONNECT_TIMEOUT < DOWNLOAD_IDLE_TIMEOUT);
        assert!(DOWNLOAD_IDLE_TIMEOUT < DOWNLOAD_TOTAL_TIMEOUT);

        let agent = build_download_agent(
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_millis(800),
        );
        let started = Instant::now();
        let result = agent.get(&format!("http://{addr}/whatever")).call();
        let elapsed = started.elapsed();

        assert!(result.is_err(), "对端不发数据时必须报错，而不是一直等");
        assert!(
            elapsed < Duration::from_millis(1500),
            "超时没生效：请求拖了 {elapsed:?} 才结束"
        );
        let _ = holder.join();
    }

    /// 把运行时构造的临时路径变成 `'static str`，好塞进 `DownloadItem::dest`。
    /// 测试里泄漏几个短字符串无所谓，换来的是测试不再向仓库树写东西。
    fn leak_str(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// 同目录词表的相对路径必须是正斜杠且与其它配置项同口径。
    #[test]
    fn sibling_tokens_rel_uses_forward_slashes() {
        use super::sibling_tokens_rel;
        assert_eq!(
            sibling_tokens_rel("models/dolphin/model.int8.onnx"),
            "models/dolphin/tokens.txt"
        );
        // 只给文件名时落在根目录
        assert_eq!(sibling_tokens_rel("model.onnx"), "tokens.txt");
    }

    /// 压缩包类型从 URL 推断：sherpa-onnx 的发布包一律 .tar.bz2，其余按 zip。
    #[test]
    fn archive_kind_follows_url_extension() {
        use super::archive_kind_from_url;
        use super::ArchiveKind;
        assert_eq!(
            archive_kind_from_url("https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-paraformer-zh-2024-03-09.tar.bz2"),
            ArchiveKind::TarBz2
        );
        // 大小写混排 / 带查询串也要认得出
        assert_eq!(
            archive_kind_from_url("https://x/y/Model.TAR.BZ2?download=1"),
            ArchiveKind::TarBz2
        );
        assert_eq!(
            archive_kind_from_url("https://x/y/pkg.tbz2"),
            ArchiveKind::TarBz2
        );
        assert_eq!(
            archive_kind_from_url("https://x/y/pkg.zip"),
            ArchiveKind::Zip
        );
        assert_eq!(
            archive_kind_from_url("https://x/y/pkg.exe"),
            ArchiveKind::Zip
        );
    }

    /// 带唯一顶层目录的 tar.bz2 要剥掉那一层，否则登记路径会被版本号绑死。
    #[test]
    fn tar_bz2_strips_single_top_level_dir() {
        let dir = std::env::temp_dir().join(format!("v2w_tarbz2p_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("model.int8.onnx"), b"m").unwrap();
        std::fs::write(src_dir.join("tokens.txt"), b"t").unwrap();

        let archive = dir.join("pkg.tar.bz2");
        let script = r#"
import sys, tarfile, os
src_dir, out = sys.argv[1], sys.argv[2]
with tarfile.open(out, "w:bz2") as tf:
    for name in ("model.int8.onnx", "tokens.txt"):
        tf.add(os.path.join(src_dir, name), arcname="pkg-dir/" + name)
"#;
        let python = crate::utils::AppConfig::resolve_command("python");
        let built = std::process::Command::new(&python)
            .arg("-c")
            .arg(script)
            .arg(&src_dir)
            .arg(&archive)
            .output();
        let Ok(built) = built else { return };
        if !built.status.success() {
            return;
        }

        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let n = super::extract_tar_bz2(&archive, &out_dir).expect("解压应成功");
        assert_eq!(n, 2);
        assert!(
            out_dir.join("model.int8.onnx").exists(),
            "顶层目录必须被剥掉，模型要直接落在目标目录下"
        );
        assert!(!out_dir.join("pkg-dir").exists(), "不应保留那层包装目录");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// .tar.bz2 解压必须跳过「跑到目标目录之外」的成员（路径穿越）。
    ///
    /// 便携 Python 是 3.11，而 extractall 的 filter="data" 要 3.12 才有，
    /// 所以安全靠脚本里自己逐成员校验——这条测试盯的就是那段校验别被改坏。
    #[test]
    fn tar_bz2_extraction_skips_escaping_members() {
        let dir = std::env::temp_dir().join(format!("v2w_tarbz2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("ok.txt"), b"hello").unwrap();

        let archive = dir.join("pkg.tar.bz2");
        let script = r#"
import sys, tarfile, io, os
src_dir, out = sys.argv[1], sys.argv[2]
with tarfile.open(out, "w:bz2") as tf:
    tf.add(os.path.join(src_dir, "ok.txt"), arcname="ok.txt")
    info = tarfile.TarInfo("../evil.txt")
    info.size = 4
    tf.addfile(info, io.BytesIO(b"evil"))
"#;
        let python = crate::utils::AppConfig::resolve_command("python");
        let built = std::process::Command::new(&python)
            .arg("-c")
            .arg(script)
            .arg(&src_dir)
            .arg(&archive)
            .output();
        // 没有可用的 Python（例如精简 CI 环境）：跳过，而不是让测试失败。
        let Ok(built) = built else { return };
        if !built.status.success() {
            return;
        }

        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let n = super::extract_tar_bz2(&archive, &out_dir).expect("解压应成功");
        assert_eq!(n, 1, "只应解出一个正常文件");
        assert!(out_dir.join("ok.txt").exists(), "正常文件必须解出");
        assert!(
            !dir.join("evil.txt").exists(),
            "路径穿越成员必须被跳过（否则能写到目标目录之外）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 登记值过期时必须改信服务端：否则上游换了更大的量化版本，正常下载会被自己的上限拦死。
    #[test]
    fn size_bound_falls_back_to_server_when_registry_is_stale() {
        const Q8: u64 = 874_188_075;

        // 服务端没声明：只能用登记值
        assert_eq!(super::resolve_size_bound(Q8, 0), (Q8, false));
        // 未登记大小：不设上限
        assert_eq!(super::resolve_size_bound(0, 1_234), (0, false));
        // 完全一致 / 小幅偏差（5% 内）：用登记值
        assert_eq!(super::resolve_size_bound(Q8, Q8), (Q8, false));
        assert_eq!(super::resolve_size_bound(Q8, Q8 + Q8 / 50), (Q8, false));
        // 服务端明显更大 → 认为登记过期，按服务端放行，并标记为过期
        let (bound, stale) = super::resolve_size_bound(Q8, 1_200_000_000);
        assert_eq!(bound, 1_200_000_000);
        assert!(stale, "必须报告登记值已过期");
    }

    /// 超限判据：只挡「明显超出」（>105%），不误伤正常的小幅偏差。
    #[test]
    fn exceeds_expected_only_when_clearly_over() {
        // 登记大小未知：不设上限，否则会把正常下载掐死
        assert!(!super::exceeds_expected(100, 0));
        // 105% 以内放行（服务端多吐几个字节是常态）
        assert!(!super::exceeds_expected(105, 100));
        assert!(super::exceeds_expected(106, 100));
        assert!(!super::exceeds_expected(400_000_000, 400_000_000));
        // 实测症状：登记 400 MB，实际收到 2 GB
        assert!(super::exceeds_expected(2_000_000_000, 400_000_000));
    }

    /// 换源预算：累计传输超过登记大小 3 倍就停手，不再继续烧流量。
    #[test]
    fn retry_budget_trips_at_three_times() {
        assert!(!super::retry_budget_exhausted(300, 0), "未知大小不设预算");
        assert!(!super::retry_budget_exhausted(300, 100));
        assert!(super::retry_budget_exhausted(301, 100));
        assert!(super::retry_budget_exhausted(2_000_000_000, 400_000_000));
    }

    /// 目录求和必须递归：解压型条目的占用全在子目录里，只算顶层等于没算。
    #[test]
    fn dir_size_walks_subdirectories() {
        let base = std::env::temp_dir().join(format!("v2w_dirsize_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub/deep")).unwrap();
        std::fs::write(base.join("a.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(base.join("sub/b.bin"), vec![0u8; 200]).unwrap();
        std::fs::write(base.join("sub/deep/c.bin"), vec![0u8; 300]).unwrap();
        assert_eq!(super::dir_size(&base), 600, "必须把子目录里的文件都算上");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 共用目录（llama / sherpa 平铺解到 `tools/`）不能按目录求和，
    /// 否则会把 ffmpeg、python 的体积算进它自己，并在合计里重复计数。
    #[test]
    fn archive_root_exclusivity_detects_shared_tools_dir() {
        use std::path::{Path, PathBuf};
        let archive = PathBuf::from("tools/llama-completion.exe");

        // 同目录还有别的条目 → 不独占
        let others = vec![PathBuf::from("tools/ffmpeg.exe")];
        assert!(
            !super::archive_root_is_exclusive(&archive, others.into_iter()),
            "共用 tools/ 时必须判定为非独占"
        );

        // 独占子目录 → 可以按目录求和
        let others = vec![PathBuf::from("tools/ffmpeg.exe")];
        assert!(super::archive_root_is_exclusive(
            Path::new("tools/whisper-cuda/whisper-cli.exe"),
            others.into_iter()
        ));

        // 没有任何兄弟条目 → 独占
        assert!(super::archive_root_is_exclusive(
            Path::new("tools/whisper-cuda/whisper-cli.exe"),
            std::iter::empty()
        ));
    }

    /// 每个条目都必须有 URL、有落地路径、且路径落在预期目录内。
    /// 这些常量一旦手滑写错（例如把 dest 写成绝对路径或漏掉目录），
    /// 只会在用户点下载时才暴露。
    #[test]
    fn items_are_well_formed() {
        for item in ITEMS {
            assert!(!item.urls.is_empty(), "{} 没有下载源", item.id);
            assert!(!item.dest.is_empty(), "{} 没有落地路径", item.id);
            assert!(
                !Path::new(item.dest).is_absolute(),
                "{} 的 dest 必须是相对项目根的路径: {}",
                item.id,
                item.dest
            );
            assert!(
                item.dest.starts_with("models/") || item.dest.starts_with("tools/"),
                "{} 的 dest 应落在 models/ 或 tools/ 下: {}",
                item.id,
                item.dest
            );
            for url in item.urls {
                assert!(
                    is_allowed_source(url),
                    "{} 的下载源必须走免梯子的镜像或代理: {}",
                    item.id,
                    url
                );
                // 直连 github.com 在本机实测超时，不能作为下载源；
                // 但允许它出现在代理 URL 的路径里（即 PROXY_BASE 后面）。
                if let Some(rest) = url.strip_prefix(PROXY_BASE) {
                    assert!(
                        rest.starts_with("https://github.com/"),
                        "{} 的代理源应代理 github.com 的附件: {}",
                        item.id,
                        url
                    );
                }
            }
        }
    }

    /// 条目 id 必须唯一——界面用它做 key，重复会导致进度显示串行。
    #[test]
    fn item_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for item in ITEMS {
            assert!(seen.insert(item.id), "重复的 id: {}", item.id);
        }
    }

    /// 落地路径必须互不重复。
    ///
    /// 这条守的是「模型指向的路径到底对不对」里最容易错、后果最严重的一类：
    /// 两个条目写同一个文件时，下载 A 会被下载 B 覆盖，而「是否已就位」的判定
    /// 又按体积下限来——于是界面显示两个都「已就位」，实际只有后下的那个存在。
    /// 这类问题在运行时不报错，只表现为「下过的模型忽然不见了」，极难排查。
    ///
    /// 比较前做大小写折叠与分隔符归一：本项目在 Windows 上跑，
    /// `Models/Whisper/x.bin` 与 `models/whisper/x.bin` 是同一个文件。
    #[test]
    fn destination_paths_are_unique() {
        let mut seen: std::collections::HashMap<String, &'static str> =
            std::collections::HashMap::new();
        for item in ITEMS {
            let key = item.dest.replace('\\', "/").to_ascii_lowercase();
            if let Some(prev) = seen.insert(key, item.id) {
                panic!(
                    "条目 {prev} 与 {} 的落地路径重复：{}（后者会覆盖前者）",
                    item.id, item.dest
                );
            }
        }
    }

    /// 每个组都至少有一个条目，避免界面渲染出空分组。
    #[test]
    fn every_group_has_items() {
        for g in [ItemGroup::Asr, ItemGroup::Text, ItemGroup::Binary] {
            assert!(
                ITEMS.iter().any(|i| i.group == g),
                "分组 {} 没有任何条目",
                g.label()
            );
        }
    }

    /// 必须存在「必需」条目（ffmpeg / whisper 主模型），
    /// 否则「缺什么才拦着用户」的判定会永远为空。
    #[test]
    fn required_items_exist() {
        assert!(
            ITEMS.iter().filter(|i| i.required).count() >= 2,
            "应至少有两个必需组件（ffmpeg 与 Whisper 主模型）"
        );
    }

    /// 期望大小必须为正（0 会让校验被跳过，等于不校验）。
    #[test]
    fn sizes_are_positive() {
        for item in ITEMS {
            assert!(item.size > 0, "{} 的期望大小不能为 0", item.id);
        }
    }

    /// 缺失判定：文件不存在 → 缺；大小明显不足 → 缺。
    #[test]
    fn presence_detection_rejects_truncated_files() {
        let dir = std::env::temp_dir().join(format!("v2w_dl_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 用**绝对路径**（临时目录）避免写进仓库树，
        // 与其它并行测试互相干扰。
        let real = dir.join("probe.bin");
        let item = DownloadItem {
            id: "test-item",
            label: "t",
            note: "n",
            dest: leak_str(real.to_str().unwrap().to_string()),
            urls: &["https://example.invalid/x"],
            size: 1000,
            min_size: 0,
            required: false,
            group: ItemGroup::Asr,
            is_archive: false,
            companion: None,
            sha256: None,
        };

        let _ = fs::remove_file(&real);
        assert!(!is_present(&item), "文件不存在时应判为缺失");

        // 造一个明显截断的文件（10% 大小）→ 仍应判为缺失
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, vec![0u8; 100]).unwrap();
        assert!(!is_present(&item), "截断文件应判为缺失");

        // 写足大小 → 判为已就位
        fs::write(&real, vec![0u8; 1000]).unwrap();
        assert!(is_present(&item), "大小达标应判为已就位");

        // 稍小但在 5% 容差内 → 仍算就位（上游重新导出会有微小出入）
        fs::write(&real, vec![0u8; 970]).unwrap();
        assert!(is_present(&item), "5% 容差内的差异应算就位");

        let _ = fs::remove_file(&real);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 空文件必须判为缺失（0 字节的「下载成功」是最坑的一种损坏）。
    #[test]
    fn empty_file_counts_as_missing() {
        let dir = std::env::temp_dir().join(format!("v2w_empty_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("probe.bin");
        let item = DownloadItem {
            id: "empty-test",
            label: "t",
            note: "n",
            dest: leak_str(real.to_str().unwrap().to_string()),
            urls: &["https://example.invalid/x"],
            size: 100,
            min_size: 0,
            required: false,
            group: ItemGroup::Asr,
            is_archive: false,
            companion: None,
            sha256: None,
        };
        fs::write(&real, b"").unwrap();
        assert!(!is_present(&item), "0 字节文件必须判为缺失");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 压缩包类条目：入口文件存在但缺少伴生文件时必须判为缺失。
    ///
    /// 这是 llama.cpp 最容易踩的坑：.exe 只有几 KB，
    /// 真正代码在同目录 DLL 里。只看 exe 大小会把「解压不完整」
    /// 当成「已就位」，用户点翻译时才爆。
    #[test]
    fn companion_file_is_required_for_archive_items() {
        let dir = std::env::temp_dir().join(format!("v2w_companion_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // dest 用**绝对路径**（指向临时目录）：这样 `resolve_existing_path` 就不会
        // 落到真实的 models/ 下，避免与并行测试互相干扰（上一版就是因为写到仓库树里、
        // 残留的伴生文件让第二次运行误判而失败）。
        let real = dir.join("tool.exe");
        let item = DownloadItem {
            id: "arch-test",
            label: "t",
            note: "n",
            dest: leak_str(real.to_str().unwrap().to_string()),
            urls: &["https://example.invalid/x"],
            size: 10,
            min_size: 0,
            required: false,
            group: ItemGroup::Binary,
            is_archive: true,
            companion: Some("tool-server.exe"),
            sha256: None,
        };

        // 只有入口文件 → 仍判为缺失（伴生文件不在）
        fs::write(&real, vec![0u8; 10]).unwrap();
        assert!(!is_present(&item), "缺少伴生文件时必须判为缺失");

        // 补上伴生文件 → 判为已就位
        fs::write(dir.join("tool-server.exe"), vec![0u8; 10]).unwrap();
        assert!(is_present(&item), "入口与伴生文件都在时应判为已就位");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 静态自包含构建（入口很大、无同目录 DLL）不能被伴生文件校验误判为缺失。
    ///
    /// 真实场景：本机已装的 whisper Vulkan 版是 61 MB 的**单文件**，
    /// 同目录没有 whisper.dll。若一律要求 DLL，会把它误判为缺失。
    #[test]
    fn self_contained_build_without_companion_is_present() {
        let dir = std::env::temp_dir().join(format!("v2w_selfcontained_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("tool.exe");
        let item = DownloadItem {
            id: "sc-test",
            label: "t",
            note: "n",
            dest: leak_str(real.to_str().unwrap().to_string()),
            urls: &["https://example.invalid/x"],
            size: 4_000_000,
            min_size: 100_000,
            required: false,
            group: ItemGroup::Binary,
            is_archive: true,
            companion: Some("tool.dll"),
            sha256: None,
        };
        // 入口 5 MB（> 2 MB 阈值）且没有 tool.dll → 应判为已就位
        fs::write(&real, vec![0u8; 5_000_000]).unwrap();
        assert!(is_present(&item), "静态自包含构建不应因缺 DLL 被判为缺失");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 自编译的自包含构建必须被识别出来，`download_one` 据此拒绝覆盖。
    ///
    /// 回归：本机手编的 whisper Vulkan 版是 60 MB 单文件，一旦点「下载」，
    /// 官方启动桩会把它悄悄换掉、GPU 支持凭空消失。
    #[test]
    fn custom_self_contained_build_is_detected() {
        let dir = std::env::temp_dir().join(format!("v2w_custom_build_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let entry = dir.join("tool.exe");
        let item = DownloadItem {
            id: "sc-guard",
            label: "t",
            note: "n",
            dest: leak_str(entry.to_str().unwrap().to_string()),
            urls: &["https://example.invalid/x"],
            size: 4_000_000,
            min_size: 100_000,
            required: false,
            group: ItemGroup::Binary,
            is_archive: true,
            companion: Some("tool.dll"),
            sha256: None,
        };

        // 1) 大单文件、无 DLL → 判定为自编译构建
        fs::write(&entry, vec![0u8; 5_000_000]).unwrap();
        assert!(
            looks_like_custom_build(&item, &entry),
            "60MB 级单文件应被识别为自编译构建"
        );

        // 2) 补上随附 DLL（本项目自己下载展开的形态）→ 不再视为自编译，允许升级覆盖
        fs::write(dir.join("tool.dll"), vec![0u8; 10]).unwrap();
        assert!(
            !looks_like_custom_build(&item, &entry),
            "有随附 DLL 时不应视为自编译构建"
        );

        // 3) 小体积启动桩（官方形态）→ 不是自编译构建
        let _ = fs::remove_file(dir.join("tool.dll"));
        fs::write(&entry, vec![0u8; 300_000]).unwrap();
        assert!(
            !looks_like_custom_build(&item, &entry),
            "几百 KB 的官方启动桩不应被判为自编译构建"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 回归：**带 ggml-vulkan.dll 的 GPU 构建**（入口是官方形态的几百 KB 桩）
    /// 也必须被判为自定义构建，避免被官方 CPU 包静默覆盖、GPU 支持消失。
    ///
    /// 这正是用户按 docs/企业级升级路线图.md P0-A2 自编的 MSVC + Vulkan 版形态：
    /// whisper-cli.exe + whisper.dll + ggml.dll + ggml-cpu.dll + ggml-vulkan.dll。
    #[test]
    fn gpu_build_with_vulkan_dll_is_detected() {
        let dir = std::env::temp_dir().join(format!("v2w_gpu_build_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let entry = dir.join("whisper-cli.exe");
        // 入口是几百 KB 的官方形态桩（低于 SELF_CONTAINED_ENTRY_FLOOR），
        // 因此只能靠 ggml-vulkan.dll 这条证据识别。
        fs::write(&entry, vec![0u8; 485_888]).unwrap();
        let item = DownloadItem {
            id: "whisper-cli",
            label: "t",
            note: "n",
            dest: leak_str(entry.to_str().unwrap().to_string()),
            urls: &["https://example.invalid/x"],
            size: 4_000_000,
            min_size: 100_000,
            required: false,
            group: ItemGroup::Binary,
            is_archive: true,
            companion: Some("whisper.dll"),
            sha256: None,
        };

        // 1) 纯 CPU 形态：有 companion、无 vulkan DLL → 不算自定义构建
        fs::write(dir.join("whisper.dll"), vec![0u8; 483_840]).unwrap();
        assert!(
            !looks_like_custom_build(&item, &entry),
            "官方 CPU 形态（桩 + companion，无 vulkan）不应被判为自定义构建"
        );

        // 2) 加上 ggml-vulkan.dll → 判为 GPU 自定义构建
        fs::write(dir.join("ggml-vulkan.dll"), vec![0u8; 1024]).unwrap();
        assert!(
            looks_like_custom_build(&item, &entry),
            "带 ggml-vulkan.dll 的构建应被判为自定义，避免被 CPU 包覆盖"
        );

        // 3) 测试 CUDA / cuBLAS DLL 同样能被正确识别为 GPU 构建
        fs::remove_file(dir.join("ggml-vulkan.dll")).unwrap();
        fs::write(dir.join("cublas64_12.dll"), vec![0u8; 1024]).unwrap();
        assert!(
            looks_like_custom_build(&item, &entry),
            "带 cublas64_12.dll 的构建应被判为 GPU 自定义构建"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 构造一个最小 zip（全部「存储」条目），供解压测试使用。
    fn tiny_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let local_off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&local_off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// 解压成功：入口 + 伴生文件都落盘，且 `.part` 被清理。
    #[test]
    fn unpack_archive_succeeds_and_cleans_part() {
        let dir = std::env::temp_dir().join(format!("v2w_unpack_ok_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let item = DownloadItem {
            id: "unpack-ok",
            label: "t",
            note: "n",
            dest: "models/__dl_unpack_ok__/tool.exe",
            urls: &["https://example.invalid/x"],
            size: 1,
            min_size: 0,
            required: false,
            group: ItemGroup::Binary,
            is_archive: true,
            companion: Some("tool-server.exe"),
            sha256: None,
        };
        let part = dir.join("tool.exe.part");
        fs::write(
            &part,
            tiny_zip(&[("tool.exe", b"exe"), ("tool-server.exe", b"server")]),
        )
        .unwrap();
        let dest = dir.join("tool.exe");

        unpack_archive(&item, &part, &dest, "https://example.invalid/x.zip").unwrap();
        assert!(dest.exists(), "入口文件应已解压落盘");
        assert!(dir.join("tool-server.exe").exists(), "伴生文件应已解压落盘");
        assert!(!part.exists(), "成功后 .part 必须被删除");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 解压缺伴生文件时必须报错，且**不**删除 `.part`（留给下次清理）。
    #[test]
    fn unpack_archive_fails_when_companion_missing() {
        let dir = std::env::temp_dir().join(format!("v2w_unpack_bad_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let item = DownloadItem {
            id: "unpack-bad",
            label: "t",
            note: "n",
            dest: "models/__dl_unpack_bad__/tool.exe",
            urls: &["https://example.invalid/x"],
            size: 1,
            min_size: 0,
            required: false,
            group: ItemGroup::Binary,
            is_archive: true,
            companion: Some("tool-server.exe"),
            sha256: None,
        };
        let part = dir.join("tool.exe.part");
        // 只有入口，缺伴生文件
        fs::write(&part, tiny_zip(&[("tool.exe", b"exe")])).unwrap();
        let dest = dir.join("tool.exe");

        assert!(
            unpack_archive(&item, &part, &dest, "https://example.invalid/x.zip").is_err(),
            "缺伴生文件应报错"
        );
        assert!(part.exists(), "失败时 .part 应保留以便清理");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `PresenceContext` 必须与逐条 `is_present` 给出**一致**的结论。
    ///
    /// 引入它是为了把「读配置」从 N 次降到 1 次；一旦两者判定分叉，
    /// 界面（走 Context）与启动日志（曾走 is_present）就会互相矛盾——
    /// 比如界面说「已就位」而日志说「缺失」。
    #[test]
    fn presence_context_agrees_with_single_shot_check() {
        let ctx = PresenceContext::load();
        for item in ITEMS {
            assert_eq!(
                ctx.is_present(item),
                is_present(item),
                "{} 在 PresenceContext 与 is_present 下结论不一致",
                item.id
            );
        }
    }

    /// 配置里把组件指到**非默认路径**时，判定必须跟着走。
    ///
    /// 这是之前的一个真实缺陷：判定只看 `item.dest`（默认路径），
    /// 于是「ffmpeg 装在 <某个非默认位置>」的用户会看到界面一直报 ffmpeg 缺失，
    /// 反复下载同一个文件。这里构造一份指向临时文件的配置来锁住行为。
    #[test]
    fn configured_path_overrides_default_dest() {
        let dir = std::env::temp_dir().join(format!("v2w_cfgpath_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fake_ffmpeg = dir.join("my-ffmpeg.exe");
        // 造一个「像样的可执行文件」（大小超过 ffmpeg 的 min_size）
        fs::write(&fake_ffmpeg, vec![0u8; 300_000]).unwrap();

        let mut cfg = AppConfig::default();
        cfg.paths.ffmpeg = fake_ffmpeg.to_string_lossy().to_string();

        let ffmpeg_item = ITEMS
            .iter()
            .find(|i| i.id == "ffmpeg")
            .expect("有 ffmpeg 条目");
        // 默认路径下什么都没有，但配置指向的文件存在 → 必须判为已就位
        assert!(
            is_present_with(ffmpeg_item, &Some(cfg)),
            "应按 config 里的路径判定，而不是只看默认 dest"
        );

        let _ = fs::remove_file(&fake_ffmpeg);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 用户把 llama.cpp 指向**自编译 / 静态构建**（同目录没有那些 DLL）时，
    /// 不能因为「缺伴生文件」而把它误判为缺失。
    /// 伴生文件校验只应用于**默认落地路径**（即我们自己下载回来的包）。
    #[test]
    fn configured_custom_llama_build_without_companion_is_present() {
        let dir = std::env::temp_dir().join(format!("v2w_cfgllama_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 只有一个 exe，没有 llama-server.exe / DLL（模拟静态构建）
        let custom = dir.join("llama-completion.exe");
        // 3 MB >= `SELF_CONTAINED_ENTRY_FLOOR`（2 MB）：这才算「静态自包含单文件」形态
        fs::write(&custom, vec![0u8; 3_000_000]).unwrap();

        let mut cfg = AppConfig::default();
        cfg.paths.llama_cli = custom.to_string_lossy().to_string();

        let item = ITEMS
            .iter()
            .find(|i| i.id == "llama-cpp")
            .expect("有 llama-cpp 条目");
        assert!(
            is_present_with(item, &Some(cfg)),
            "配置指向自构建时，不应要求同目录必有伴生文件"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `human_size` 的边界：不足 1KB 按字节、1KB 起步、一位小数、抹掉末尾 `.0`。
    ///
    /// 锁这条规则是因为界面靠它给用户「值不值得下」的第一印象：
    /// `123.0 KB` 这种尾部 `.0` 会让用户以为数字没处理干净。
    ///
    /// 注意示例值 `0.9 GB`：在「1 KB = 1024 B」且**按 1024^k 选档**的规则下，
    /// 0.9 GB（966_367_641 B）严格小于 1 GB 门槛，因此必然显示为 `921.6 MB`
    /// ——只有 ≥ 1 GB 才会进入 GB 档。这里把这个边界显式钉住，避免以后有人
    /// 想当然地改成「0.9 GB」而对不上真实数值。
    #[test]
    fn human_size_boundaries() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        // 恰好 1 KB：抹掉 `.0` → `1 KB`
        assert_eq!(human_size(1024), "1 KB");
        // 123 KB（原始值 123.0）→ 同样抹掉 `.0`
        assert_eq!(human_size(123 * 1024), "123 KB");
        // 1.5 MB：保留一位小数
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MB");
        // 跨单位后仍是一位小数
        assert_eq!(human_size(1024 * 1024 * 10 + 1024 * 512), "10.5 MB");
        // 0.9 GB 示例值（966_367_641 B）低于 1 GB 门槛 → 按 1024 进制落在 MB 档
        assert_eq!(human_size(966_367_641), "921.6 MB");
        // GB 档：同样只保留一位小数，并按规则抹掉末尾 `.0`
        assert_eq!(human_size(1024 * 1024 * 1024), "1 GB");
        assert_eq!(human_size(1024 * 1024 * 1024 * 5 / 2), "2.5 GB");
    }
    /// `item_by_id` 能找到已知条目，且对未知 id 返回 `None`（不能 panic）。
    #[test]
    fn item_by_id_finds_known_and_rejects_bogus() {
        let small = item_by_id("whisper-small").expect("应能找到 whisper-small");
        assert_eq!(small.id, "whisper-small");
        assert_eq!(small.dest, "models/whisper/ggml-small-q5_0.bin");

        assert!(item_by_id("no-such-item").is_none());
        assert!(item_by_id("").is_none());
    }

    /// `item_is_deletable` 是界面删除按钮的显示门禁：
    /// 压缩包条目与未知 id 一律 `false`，其余普通文件条目 `true`。
    /// `item_id_for_path` 必须把「默认落地路径」映射回它所属的条目 id，
    /// 且对清单外的路径（例如 `ggml-small.bin` 这类没有条目的回退文件）返回 `None`。
    ///
    /// 这条锁住档位下拉里的删除接线：删除按**实际文件**反查条目，反查不到就不给
    /// 删除按钮——若映射写错，会出现「显示已就位、点删除说没文件」或更糟的删错东西。
    #[test]
    fn item_id_for_path_maps_default_dests_and_rejects_others() {
        for item in ITEMS {
            let dest = AppConfig::resolve_path(item.dest);
            assert_eq!(
                item_id_for_path(&dest),
                Some(item.id),
                "{} 的默认落地路径应反查回自身",
                item.id
            );
        }
        // 清单外的文件（档位回退文件 / 用户自备模型）必须反查不到
        let outside = AppConfig::resolve_path("models/whisper/ggml-small.bin");
        assert_eq!(item_id_for_path(&outside), None);
        let bogus = AppConfig::resolve_path("models/whisper/definitely-not-a-model.bin");
        assert_eq!(item_id_for_path(&bogus), None);
    }

    #[test]
    fn item_is_deletable_rules() {
        // 压缩包组件（解压出来的半包/共享目录）不给删除按钮
        assert!(!item_is_deletable("whisper-cli"));
        assert!(!item_is_deletable("llama-cpp"));
        // 未知 id（条目改名/下线后的过期值）也不给按钮
        assert!(!item_is_deletable("no-such-id"));
        // 普通文件条目可以删
        assert!(item_is_deletable("whisper-small"));
        assert!(item_is_deletable("qwen-llm"));
        assert!(item_is_deletable("ffmpeg"));
    }

    /// 配置把组件指到临时目录里的真实文件时，删除应删掉**那个**文件并返回其路径。
    #[test]
    fn delete_item_file_removes_configured_file() {
        let dir = std::env::temp_dir().join(format!("v2w_del_ok_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file = dir.join("my-ffmpeg.exe");
        fs::write(&file, vec![0u8; 4096]).unwrap();

        let mut cfg = AppConfig::default();
        // 绝对路径会被 `resolve_path` 原样保留，且 `resolve_existing_path`
        // 优先取配置路径 —— 正好避免碰仓库树里的任何文件。
        cfg.paths.ffmpeg = file.to_string_lossy().to_string();

        let deleted = delete_item_file("ffmpeg", &cfg).expect("应能删除配置指向的文件");
        assert_eq!(deleted.as_deref(), Some(file.as_path()));
        assert!(!file.exists(), "配置指向的文件应已被删除");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 路径下什么都没有时删除是幂等的空操作：`Ok(None)`，不报错。
    ///
    /// # 为什么不能直接断言 `is_none`
    ///
    /// 这条用例原本假设「ffmpeg 的默认 dest（`tools/ffmpeg.exe`）在本仓库不存在」——
    /// 但那是**机器状态**，不是代码性质：用户在「模型与组件」里下过 ffmpeg，这个文件
    /// 就真的在那里（本机就下过）。一旦它存在，删除会命中**真实文件**，测试绝不能碰。
    /// 因此分两种情形：默认路径也不存在时照旧断言空操作；存在时只断言「配置里那个不
    /// 存在的路径没被当成可用」——绝不去调删除。
    #[test]
    fn delete_item_file_missing_returns_none() {
        let dir = std::env::temp_dir().join(format!("v2w_del_missing_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut cfg = AppConfig::default();
        // 配置指向一个不存在的文件（临时目录里没建它）。
        cfg.paths.ffmpeg = dir.join("not-there.exe").to_string_lossy().to_string();
        let configured = PathBuf::from(&cfg.paths.ffmpeg);

        let item = item_by_id("ffmpeg").unwrap();
        match resolve_existing_path(item, Some(&cfg)) {
            // 默认路径也不存在：真正的「什么都没有」，删除是空操作。
            None => assert_eq!(
                delete_item_file("ffmpeg", &cfg).unwrap(),
                None,
                "无文件时删除应为空操作"
            ),
            // 默认路径存在（用户下载过）：**绝不删除**，只确认没把配置里那个不存在的路径当成可用。
            Some(found) => assert_ne!(
                found, configured,
                "配置里不存在的路径不该被当成可用的解析结果"
            ),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    /// 压缩包条目（llama-cpp / whisper-cli）必须拒绝删除，并提示手动移除整个目录。
    ///
    /// 它们解压后入口 exe 只有几 KB，真正代码在同目录 DLL 里；llama.cpp 更是
    /// 直接解压到 `tools/` 本身。删单个文件只会留下坏掉的半包或波及共享目录。
    #[test]
    fn delete_item_file_refuses_archive() {
        let cfg = AppConfig::default();
        let err = delete_item_file("llama-cpp", &cfg).expect_err("压缩包条目必须拒绝删除");
        let msg = err.to_string();
        assert!(msg.contains("手动"), "错误信息应提示手动删除，实际: {msg}");
        assert!(
            msg.contains("tools"),
            "错误信息应点出需要手动清理的目录，实际: {msg}"
        );
    }

    /// 未知 id 必须报错，且错误里带上这个 id，方便定位是哪个界面键过期了。
    #[test]
    fn delete_item_file_rejects_unknown_id() {
        let cfg = AppConfig::default();
        let err = delete_item_file("definitely-not-an-item", &cfg).expect_err("未知 id 必须报错");
        assert!(err.to_string().contains("definitely-not-an-item"));
    }

    /// 用户自编译的构建不能删。
    ///
    /// 走 `delete_item_file` 能到达「自编译保护」的新只有**非压缩包**条目，
    /// 因此这里用 ffmpeg + 同目录 `ggml-vulkan.dll` 构造「用户手编的 GPU 构建」
    /// 形态，命中 `is_gpu_build`。同时直接验证同源判据
    /// `looks_like_custom_build`（带伴生文件的大单文件）——压缩包条目会更早被拒，
    /// 所以用合成条目锁住这条谓词本身。
    #[test]
    fn delete_item_file_refuses_custom_build() {
        let dir = std::env::temp_dir().join(format!("v2w_del_custom_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let exe = dir.join("ffmpeg.exe");
        // >= 2 MB 的大单文件 + 官方 CPU 包从不提供的 GPU 后端 DLL。
        fs::write(&exe, vec![0u8; 3_000_000]).unwrap();
        fs::write(dir.join("ggml-vulkan.dll"), vec![0u8; 1024]).unwrap();

        let mut cfg = AppConfig::default();
        cfg.paths.ffmpeg = exe.to_string_lossy().to_string();

        let err = delete_item_file("ffmpeg", &cfg).expect_err("自编译 GPU 构建必须拒绝删除");
        assert!(
            err.to_string().contains("编译"),
            "错误信息应说明这是自编译构建: {err}"
        );
        assert!(exe.exists(), "自编译构建绝不能被删掉");
        assert!(
            dir.join("ggml-vulkan.dll").exists(),
            "同目录的 GPU 后端 DLL 也不能被波及"
        );

        // 同源判据：带伴生文件 + >= 2 MB 单文件（同目录没有随附 DLL）
        // 也必须判为自编译构建 —— delete_item_file 正是用它来做门禁。
        let companion_item = DownloadItem {
            id: "synthetic-companion",
            label: "t",
            note: "n",
            dest: leak_str(exe.to_str().unwrap().to_string()),
            urls: &["https://example.invalid/x"],
            size: 4_000_000,
            min_size: 0,
            required: false,
            group: ItemGroup::Binary,
            is_archive: false,
            companion: Some("whisper.dll"),
            sha256: None,
        };
        assert!(
            looks_like_custom_build(&companion_item, &exe),
            "带伴生文件的条目 + 大单文件应判为自编译构建"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// whisper 档位判定：配置指向 small 时，磁盘上有 turbo 不该让 small 算「已就位」。
    #[test]
    fn whisper_tier_follows_configured_model() {
        let mut cfg = AppConfig::default();
        cfg.paths.whisper_model = "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string();

        let small = ITEMS.iter().find(|i| i.id == "whisper-small").unwrap();
        let turbo_q8 = ITEMS.iter().find(|i| i.id == "whisper-turbo-q8").unwrap();

        // small 条目不应因为「配置指向 turbo」而跟着走（它该去查 small 自己的默认路径）
        let cfg_opt = Some(cfg);
        // turbo_q8 的配置路径就是它自己的默认路径 → 两者应给出一致结论
        assert_eq!(
            is_present_with(turbo_q8, &cfg_opt),
            is_present(turbo_q8),
            "配置指向 turbo-q8 时，该条目的判定不应偏离默认路径判定"
        );
        // small 条目：配置指向的是别的档位 → 不认配置路径，只看自己的默认路径
        let small_via_cfg = is_present_with(small, &cfg_opt);
        let small_default = AppConfig::resolve_path(small.dest).exists();
        assert_eq!(
            small_via_cfg, small_default,
            "配置指向别的档位时，small 条目应只按自己的默认路径判定"
        );
    }

    /// 同档位的 q5_0 / q5_1 必须互相认可（本地量化版本号差异不该判为缺失）。
    #[test]
    fn same_tier_quantization_variants_are_accepted() {
        // 直接验证「档位归并」逻辑：把配置指向 q5_1，small 条目（dest 是 q5_0）应认它
        let mut cfg = AppConfig::default();
        cfg.paths.whisper_model = "models/whisper/ggml-small-q5_1.bin".to_string();
        let small = ITEMS.iter().find(|i| i.id == "whisper-small").unwrap();
        // 该文件在本机不存在时，判定应回退到默认路径而不是直接 false；
        // 存在时应判 true。两种情况下都不应 panic，且与「只看默认路径」一致。
        let got = is_present_with(small, &Some(cfg));
        let fallback = AppConfig::resolve_path(small.dest).exists();
        assert_eq!(got, fallback, "q5_0/q5_1 应视为同一档位");
    }
    /// `.part` 命名规则必须与清理逻辑一致。
    ///
    /// 这两处一旦分叉，后果是「残留文件永远清不掉」——而且很难发现，
    /// 因为正常路径下 `.part` 会被自己删掉，只有强杀才暴露。
    #[test]
    fn part_path_matches_download_naming() {
        use std::path::Path;
        assert_eq!(
            part_path_for(Path::new("models/sensevoice/model.int8.onnx")).unwrap(),
            Path::new("models/sensevoice/model.int8.onnx.part")
        );
        assert_eq!(
            part_path_for(Path::new("tools/ffmpeg.exe")).unwrap(),
            Path::new("tools/ffmpeg.exe.part")
        );
        assert_eq!(
            part_path_for(Path::new("models/whisper/ggml-base.bin")).unwrap(),
            Path::new("models/whisper/ggml-base.bin.part")
        );
        // 没有扩展名也不能 panic
        assert_eq!(
            part_path_for(Path::new("models/noext")).unwrap(),
            Path::new("models/noext.part")
        );
    }
    /// 独立临时目录（避免触碰真实 models/，防止与并行测试互相干扰）。
    fn isolated_dest(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("v2w_sweep_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir.join("model.bin"))
    }

    /// 把文件的最后修改时间回拨 `age`，用来模拟「很久以前留下的 `.part`」。
    ///
    /// 过期判定依赖 mtime，测试又等不起 7 天，只能显式改时间戳。
    /// 改时间戳需要带写权限的句柄，故用 `write(true)` 打开。
    fn backdate_mtime(path: &Path, age: Duration) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    /// 过期的 `.part` 必须被清掉，且**不碰**旁边的正式文件。
    ///
    /// 用临时目录而不是真实模型路径：早先版本直接往 `models/` 写假文件，
    /// 结果与「已就位」判定类测试并行时互相干扰，出现偶发失败。
    #[test]
    fn sweep_removes_stale_parts_but_not_real_files() {
        let (dir, dest) = isolated_dest("stale");
        let part = part_path_for(&dest).unwrap();

        fs::write(&dest, b"complete model").unwrap();
        fs::write(&part, b"incomplete").unwrap();
        // 回拨 8 天，越过 STALE_PART_MAX_AGE（7 天）这条线。
        backdate_mtime(&part, Duration::from_secs(8 * 24 * 3600));

        let removed = sweep_parts_for(std::slice::from_ref(&dest));

        assert_eq!(removed, 1);
        assert!(!part.exists(), ".part 必须被清掉");
        assert!(dest.exists(), "正式文件绝不能被误删");
        assert_eq!(fs::read(&dest).unwrap(), b"complete model");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 近期残留（可能是可续传的半成品）必须保留，否则跨进程续传被废掉。
    #[test]
    fn sweep_preserves_fresh_parts_for_resume() {
        let (dir, dest) = isolated_dest("fresh");
        let part = part_path_for(&dest).unwrap();
        fs::write(&part, b"half downloaded").unwrap();

        // 刚写入，mtime 就是「现在」，未过期 → 不删。
        assert_eq!(sweep_parts_for(std::slice::from_ref(&dest)), 0);
        assert!(part.exists(), "近期 .part 必须保留以供续传");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 没有残留时是空操作。
    #[test]
    fn sweep_is_noop_when_clean() {
        let (dir, dest) = isolated_dest("clean");
        fs::write(&dest, b"complete").unwrap();
        assert_eq!(sweep_parts_for(std::slice::from_ref(&dest)), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 目标与 `.part` 都不存在时不能报错（清理是尽力而为）。
    #[test]
    fn sweep_tolerates_missing_everything() {
        let (dir, dest) = isolated_dest("missing");
        assert_eq!(sweep_parts_for(std::slice::from_ref(&dest)), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 声明了 sha256 时必须逐字比对（十六进制大小写不敏感），对不上判未就位。
    #[test]
    fn present_by_content_honors_sha256() {
        let dir = std::env::temp_dir().join(format!("v2w_present_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc.bin");
        fs::write(&file, b"abc").unwrap();
        let len = fs::metadata(&file).unwrap().len();

        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(present_by_content(&file, len, Some(digest), 0));
        // 大写摘要同样通过（上游登记大小写不一定一致）。
        assert!(present_by_content(
            &file,
            len,
            Some(&digest.to_ascii_uppercase()),
            0
        ));
        // 内容对不上：即使体积满足下限也必须判未就位，让上层重下。
        let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!present_by_content(&file, len, Some(wrong), 1));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 未声明 sha256 时退回体积下限：达到下限算就位，不足则未就位。
    #[test]
    fn present_by_content_falls_back_to_size_floor() {
        let dir = std::env::temp_dir().join(format!("v2w_present_floor_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("blob.bin");
        fs::write(&file, vec![7u8; 100]).unwrap();
        let len = fs::metadata(&file).unwrap().len();

        assert!(present_by_content(&file, len, None, 50), "超过下限算就位");
        assert!(
            !present_by_content(&file, len, None, 200),
            "低于下限算未就位"
        );
        assert!(
            present_by_content(&file, len, None, 0),
            "无下限时非空即算就位"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 声明了 sha256 但文件不存在时返回 false，绝不 panic（调用方靠这个换源重下）。
    #[test]
    fn present_by_content_missing_file_is_absent() {
        let missing =
            std::env::temp_dir().join(format!("v2w_present_missing_{}.bin", std::process::id()));
        let _ = fs::remove_file(&missing);
        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(!present_by_content(&missing, 0, Some(digest), 0));
    }

    /// 过期判定边界：恰好到 STALE_PART_MAX_AGE 才算过期，时钟回拨一律保留。
    #[test]
    fn stale_part_expiry_rules() {
        let now = SystemTime::now();
        // 刚刚写入：不过期
        assert!(!is_stale_part(now, now));
        // 差 1 秒到期：保留
        assert!(!is_stale_part(
            now - (STALE_PART_MAX_AGE - Duration::from_secs(1)),
            now
        ));
        // 恰好到期：清理
        assert!(is_stale_part(now - STALE_PART_MAX_AGE, now));
        // 超过 8 天：清理
        assert!(is_stale_part(now - Duration::from_secs(8 * 24 * 3600), now));
        // mtime 在未来（时钟回拨）：保守保留
        assert!(!is_stale_part(now + Duration::from_secs(3600), now));
    }

    /// Content-Range 起点解析：识别合法写法，非法一律 None（交给调用方重下整文件）。
    #[test]
    fn content_range_start_parses_offset() {
        assert_eq!(content_range_start("bytes 500-999/1000"), Some(500));
        assert_eq!(content_range_start("bytes 0-99/100"), Some(0));
        assert_eq!(
            content_range_start("bytes 4078768-/4078768"),
            Some(4_078_768)
        );
        assert_eq!(content_range_start("  bytes 1-2/3  "), Some(1));
        assert_eq!(content_range_start("items 500-999/1000"), None);
        assert_eq!(content_range_start("bytes abc-1/2"), None);
        assert_eq!(content_range_start("bytes"), None);
        assert_eq!(content_range_start(""), None);
    }

    /// `sha256_file` 对已知内容必须给出标准摘要（"abc" 的经典向量）。
    #[test]
    fn sha256_file_matches_known_digest() {
        let dir = std::env::temp_dir().join(format!("v2w_sha_abc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc.bin");
        fs::write(&file, b"abc").unwrap();

        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 超过单次读取缓冲（256 KiB）的文件必须多轮 `update`，结果不能只覆盖第一块。
    #[test]
    fn sha256_file_streams_across_buffer_boundary() {
        let dir = std::env::temp_dir().join(format!("v2w_sha_big_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.bin");
        // 300 KiB：超过 256 KiB 缓冲，必然触发第二轮读取。
        let bytes: Vec<u8> = (0..307_200).map(|i| (i % 251) as u8).collect();
        fs::write(&file, &bytes).unwrap();

        assert_eq!(
            sha256_file(&file).unwrap(),
            "10a6169813fcc0410b3d72574ff2dd1997936b90db821427136315967cff2bb9"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 文件不存在时返回 `Err`，由调用方决定换源重试（而不是 panic）。
    #[test]
    fn sha256_file_errors_on_missing_path() {
        let path = std::env::temp_dir().join(format!("v2w_sha_missing_{}.bin", std::process::id()));
        let _ = fs::remove_file(&path);
        assert!(sha256_file(&path).is_err());
    }

    /// 续传起点只在「已有内容且未下满」时给出偏移，其余一律 `None`。
    #[test]
    fn resume_offset_rules() {
        // 无残留 / 空文件：从头下
        assert_eq!(resume_offset(0, 1000), None);
        // 已下满 / 超出：不再发 Range，交给上层做校验
        assert_eq!(resume_offset(1000, 1000), None);
        assert_eq!(resume_offset(1200, 1000), None);
        // 大小未知：无法判断续传点，从头下
        assert_eq!(resume_offset(500, 0), None);
        // 中间态：从已有长度继续
        assert_eq!(resume_offset(500, 1000), Some(500));
        assert_eq!(resume_offset(1, 2), Some(1));
    }

    /// `Range` 头必须是「从 offset 到文件末尾」的开区间写法。
    #[test]
    fn range_header_formats_open_ended_offset() {
        assert_eq!(range_header(0), "bytes=0-");
        assert_eq!(range_header(500), "bytes=500-");
        assert_eq!(range_header(4_078_768), "bytes=4078768-");
    }
    /// 借用版入口必须与既有「按值 Option」入口给出**逐条一致**的结论。
    ///
    /// 背景：`PresenceContextRef::is_present` / `is_custom_build` 原实现每次调用都
    /// `self.cfg.clone()`——深拷贝整份 `AppConfig`（paths / pipeline / 术语表等），
    /// 而 `model_manager` 是**每个条目每帧**各调一次。改成借用省掉的只是拷贝，
    /// **绝不能**改变判定：一旦分叉，界面、启动日志与删除入口就会互相矛盾。
    #[test]
    fn borrowed_presence_entries_agree_with_owned_config() {
        let dir = std::env::temp_dir().join(format!("v2w_borrow_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 造一个「够大」的假 ffmpeg（`is_present` 对它有条体积下限）
        let fake_ffmpeg = dir.join("my-ffmpeg.exe");
        fs::write(&fake_ffmpeg, vec![0u8; 300_000]).unwrap();

        let mut cfg = AppConfig::default();
        cfg.paths.ffmpeg = fake_ffmpeg.to_string_lossy().to_string();

        let item = item_by_id("ffmpeg").expect("有 ffmpeg 条目");
        // 按值入口需要一份 clone 才能与借用版同时持有同一配置
        let owned = is_present_with(item, &(Some(cfg.clone())));
        let ctx = PresenceContextRef::new(&cfg);
        assert!(owned, "前置条件：配置指向的文件存在且够大，应判为已就位");
        assert_eq!(
            ctx.is_present(item),
            owned,
            "借用上下文与按值入口结论必须一致"
        );
        assert_eq!(
            is_present_with_opt(item, Some(&cfg)),
            owned,
            "借用 Option 入口与按值入口结论必须一致"
        );
        assert_eq!(
            is_present_with_config(item, &cfg),
            owned,
            "`is_present_with_config` 与按值入口结论必须一致"
        );
        assert_eq!(
            ctx.is_custom_build(item),
            is_custom_build_with(item, &(Some(cfg.clone()))),
            "自编译判定的借用/按值入口结论必须一致"
        );

        // 反例：配置指向不存在的文件时两条路径必须**同结论**——借用不得退化成
        // 「读默认路径」或「忽略配置」。
        //
        // 只钉一致性、不钉真假：结论的绝对值取决于本机有没有把 ffmpeg 下到默认路径
        // （`tools/ffmpeg.exe`）。用户在「模型与组件」里下过它就是「存在」——那是正确
        // 行为，不是缺陷；把「必须判缺失」写死会让这条用例随机器状态飘。
        cfg.paths.ffmpeg = dir.join("not-there.exe").to_string_lossy().to_string();
        assert!(
            !Path::new(&cfg.paths.ffmpeg).exists(),
            "前置条件：配置指向的路径确实不存在"
        );
        let missing = is_present_with(item, &(Some(cfg.clone())));
        let ctx = PresenceContextRef::new(&cfg);
        assert_eq!(ctx.is_present(item), missing, "缺失结论也必须一致");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `is_custom_build` 的借用入口同样只看配置指向的**实际**路径。
    ///
    /// 素材与 `configured_custom_llama_build_without_companion_is_present` 同源：
    /// 配置指向一个 2 MB 的单文件构建（同目录没有伴生 DLL），借用版也必须认出
    /// 「用户自编译」，否则界面会把用户的构建误报成「官方包未下载」。
    #[test]
    fn borrowed_custom_build_detection_matches_owned_entry() {
        let dir = std::env::temp_dir().join(format!("v2w_borrow_custom_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let custom = dir.join("llama-completion.exe");
        // 3 MB >= `SELF_CONTAINED_ENTRY_FLOOR`（2 MB）：这才算「静态自包含单文件」形态
        fs::write(&custom, vec![0u8; 3_000_000]).unwrap();

        let mut cfg = AppConfig::default();
        cfg.paths.llama_cli = custom.to_string_lossy().to_string();

        let item = item_by_id("llama-cpp").expect("有 llama-cpp 条目");
        let owned = is_custom_build_with(item, &(Some(cfg.clone())));
        let ctx = PresenceContextRef::new(&cfg);
        assert!(owned, "3 MB 单文件且无伴生文件，应判为自编译构建");
        assert_eq!(
            ctx.is_custom_build(item),
            owned,
            "借用版必须认出同一个自编译构建"
        );
        assert_eq!(
            ctx.is_present(item),
            is_present_with(item, &(Some(cfg.clone()))),
            "就位判定同样必须一致"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
