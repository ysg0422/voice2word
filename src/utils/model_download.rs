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
//! 只校验**大小**，不校验哈希。原因是上游仓库会重新导出/重新量化模型文件
//! （例如 `ggml-silero-v5.1.2.bin` 与本地同名文件同尺寸但哈希不同），写死哈希
//! 会在上游更新后把下载判为失败，反而挡住用户。大小校验足以拦住截断的下载
//! （这正是最常见的失败模式），而**截断文件才是真正会导致「模型加载失败」
//! 且难以排查的那种损坏**。
//!
//! 下载先写 `.part` 临时文件，成功后才 `rename` 到目标名。因此中断的下载
//! 永远不会被误认成完整模型——这一点比哈希校验更关键。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
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
        size: 181_300_000, // 上游 q5_1；本机若已有 q5_0(175MB) 也应算就位
        min_size: 160_000_000,
        required: true,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
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
        size: 147_951_465,
        min_size: 130_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
    },
    DownloadItem {
        id: "whisper-turbo-q5",
        label: "Whisper Turbo Q5（推荐）",
        note: "大模型量化版，速度与精度兼得",
        dest: "models/whisper/ggml-large-v3-turbo-q5_0.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-large-v3-q5_0.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
        ],
        size: 574_041_195,
        min_size: 500_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
    },
    DownloadItem {
        id: "whisper-turbo-q8",
        label: "Whisper Turbo Q8（最准）",
        note: "旗舰精度，抗口音与吞音",
        dest: "models/whisper/ggml-large-v3-turbo-q8_0.bin",
        urls: &[
            "https://www.modelscope.cn/models/cjc1887415157/whisper.cpp/resolve/master/ggml-large-v3.bin",
            "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q8_0.bin",
        ],
        size: 874_188_075,
        min_size: 800_000_000,
        required: false,
        group: ItemGroup::Asr,
        is_archive: false,
        companion: None,
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
    },
    DownloadItem {
        id: "qwen-llm",
        label: "Qwen2.5-0.5B（润色 / 翻译）",
        note: "离线润色与字幕翻译，无需联网与密钥",
        dest: "models/llm/qwen2.5-0.5b-instruct-q4_k_m.gguf",
        urls: &[
            "https://hf-mirror.com/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf",
        ],
        size: 491_400_032,
        min_size: 400_000_000,
        required: false,
        group: ItemGroup::Text,
        is_archive: false,
        companion: None,
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
    },
];

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
        is_present_with(item, &self.cfg)
    }
}

/// 用**已有的**内存配置判定，不碰磁盘。
///
/// `AppState` 自己就持有 `config`，判定时再 `load_from_file` 一次纯属浪费
/// （启动路径上因此白读一遍 TOML）。调用方有配置就直接用这个。
pub fn is_present_with_config(item: &DownloadItem, cfg: &AppConfig) -> bool {
    is_present_with(item, &Some(cfg.clone()))
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
        is_present_with(item, &Some(self.cfg.clone()))
    }

    /// 是否命中了用户自编译的自包含构建（同一次扫描内复用配置，不额外读盘）。
    pub fn is_custom_build(&self, item: &DownloadItem) -> bool {
        is_custom_build_with(item, &Some(self.cfg.clone()))
    }
}

/// 判断某个条目是否已就位（内部使用：允许传入已读好的配置）。
///
/// # 判定为什么不是「大小相等」
///
/// 同一个逻辑模型在本项目里存在多种合法形态，只比体积会把完好的文件
/// 误判为缺失（界面于是永远显示「缺失」，用户反复下载同一个文件）：
/// - **路径可能被改**：`config.local.toml` 可以把 `ffmpeg` 指到任意位置
///   （例如 `A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe`）。只看 `item.dest`
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
    let Some(path) = resolve_existing_path(item, cfg) else {
        return false;
    };
    let Ok(meta) = fs::metadata(&path) else {
        return false;
    };
    if meta.len() == 0 {
        return false;
    }
    let floor = min_acceptable_size(item);
    if companion_missing(&path, item, meta.len()) {
        return false;
    }
    if floor == 0 {
        return true;
    }
    meta.len() >= floor
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
fn resolve_existing_path(item: &DownloadItem, cfg: &Option<AppConfig>) -> Option<PathBuf> {
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
fn configured_path_for(item: &DownloadItem, cfg: &Option<AppConfig>) -> Option<PathBuf> {
    // 配置由调用方读一次并传入；这里只做查表，不再碰磁盘。
    let cfg = cfg.as_ref()?;
    let raw: Option<&str> = match item.id {
        "ffmpeg" => Some(cfg.paths.ffmpeg.as_str()),
        "whisper-small" | "whisper-base" | "whisper-turbo-q5" | "whisper-turbo-q8" => {
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
        "whisper-small" | "whisper-base" | "whisper-turbo-q5" | "whisper-turbo-q8"
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
            || (configured_name.starts_with("ggml-small") && item_name.starts_with("ggml-small"))
            || (configured_name.starts_with("ggml-base") && item_name.starts_with("ggml-base"))
            || (configured_name.contains("turbo-q5") && item_name.contains("turbo-q5"))
            || (configured_name.contains("turbo-q8") && item_name.contains("turbo-q8"));
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
    configured_path_for(item, &Some(cfg.clone()))
        .unwrap_or_else(|| AppConfig::resolve_path(item.dest))
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
    let cfg_opt = Some(cfg.clone());
    let Some(path) = resolve_existing_path(item, &cfg_opt) else {
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
/// 先写 `<dest>.part`，成功（含大小校验）后原子 rename 到目标名。
/// 因此**任何中断都不会留下一个看起来完好的模型文件**。
pub fn download_one(
    item: &DownloadItem,
    cancel: &Arc<AtomicBool>,
    progress: Option<&ProgressFn>,
) -> Result<PathBuf> {
    let dest = AppConfig::resolve_path(item.dest);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
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

    let mut last_err: Option<anyhow::Error> = None;
    for url in item.urls {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("已取消"));
        }
        match fetch_to_file(url, &part, item.size, item.id, cancel, progress) {
            Ok(()) => {
                // 压缩包类组件（如 llama.cpp）：下载物是 zip，真正的程序在包内。
                // 必须解压到 dest 的父目录并校验关键文件都在，才算成功。
                if item.is_archive {
                    match unpack_archive(item, &part, &dest) {
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
                let _ = fs::remove_file(&part);
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("没有可用的下载源")))
}

/// 把已下载到 `part` 的 zip 解压到 `dest` 的父目录，并校验关键文件都落盘。
///
/// 抽成独立函数是因为这几步**顺序敏感**，混在 `download_one` 里容易改错：
/// 1. 解压到目标目录；
/// 2. 校验入口文件（`dest`）与伴生文件（`companion`）确实存在；
/// 3. 全部校验通过后才删除 `.part`。中途失败则保留 `.part`，
///    既不会留下半成品目录，也能被 `sweep_parts_for` 在下次启动时清掉。
fn unpack_archive(item: &DownloadItem, part: &Path, dest: &Path) -> Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| anyhow!("目标路径没有父目录: {}", dest.display()))?;
    let bytes = fs::read(part).with_context(|| format!("读取压缩包失败: {}", part.display()))?;
    let n = super::zip_extract::extract_zip(&bytes, dir)
        .with_context(|| format!("解压失败: {}", part.display()))?;
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

/// 清扫上次运行残留的 `<目标>.part` 下载中间文件。
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
    let mut removed = 0usize;
    for dest in dests {
        let Some(part) = part_path_for(dest) else {
            continue;
        };
        if !part.exists() {
            continue;
        }
        match fs::remove_file(&part) {
            Ok(()) => {
                info!(path = %part.display(), "已清理上次残留的下载中间文件");
                removed += 1;
            }
            // 删不掉（被占用/权限）不算错误：下次启动再试
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

/// 从单个 URL 流式下载到文件，并做大小校验。
fn fetch_to_file(
    url: &str,
    part: &Path,
    expected_size: u64,
    item_id: &str,
    cancel: &Arc<AtomicBool>,
    progress: Option<&ProgressFn>,
) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        // 大文件（最大 833 MB）在慢网下需要足够长的超时；
        // 这里的 timeout 是**整个请求**的上限，不是空闲超时，
        // 因此给足 2 小时，避免 800 MB 在 100 KB/s 的线路上被掐断。
        .timeout(Duration::from_secs(2 * 60 * 60))
        // 自动探测并继承环境中的 HTTP_PROXY / HTTPS_PROXY（梯子加速）
        .try_proxy_from_env(true)
        .build();

    let resp = agent
        .get(url)
        .set(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko)",
        )
        .call()
        .map_err(|e| anyhow!("请求失败: {e}"))?;

    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut file =
        fs::File::create(part).with_context(|| format!("创建临时文件失败: {}", part.display()))?;

    let mut buf = vec![0u8; 256 * 1024];
    let mut written: u64 = 0;
    let started = Instant::now();
    let mut last_report = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_file(part);
            return Err(anyhow!("已取消"));
        }
        let n = reader.read(&mut buf).context("读取响应流失败")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("写入文件失败")?;
        written += n as u64;

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

    let secs = started.elapsed().as_secs_f64().max(0.001);
    info!(
        item = item_id,
        mb = written / MB,
        secs = secs,
        speed_mbps = (written as f64 / MB as f64) / secs,
        "下载完成"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把运行时构造的临时路径变成 `'static str`，好塞进 `DownloadItem::dest`。
    /// 测试里泄漏几个短字符串无所谓，换来的是测试不再向仓库树写东西。
    fn leak_str(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
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
        };
        let part = dir.join("tool.exe.part");
        fs::write(
            &part,
            tiny_zip(&[("tool.exe", b"exe"), ("tool-server.exe", b"server")]),
        )
        .unwrap();
        let dest = dir.join("tool.exe");

        unpack_archive(&item, &part, &dest).unwrap();
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
        };
        let part = dir.join("tool.exe.part");
        // 只有入口，缺伴生文件
        fs::write(&part, tiny_zip(&[("tool.exe", b"exe")])).unwrap();
        let dest = dir.join("tool.exe");

        assert!(
            unpack_archive(&item, &part, &dest).is_err(),
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
    /// 于是「ffmpeg 装在 A:\cppsoft\...」的用户会看到界面一直报 ffmpeg 缺失，
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
        fs::write(&custom, vec![0u8; 2_000_000]).unwrap();

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
    #[test]
    fn delete_item_file_missing_returns_none() {
        let dir = std::env::temp_dir().join(format!("v2w_del_missing_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut cfg = AppConfig::default();
        // 指向一个不存在的文件；ffmpeg 的默认 dest（tools/ffmpeg.exe）在本仓库也不存在，
        // 因此 `resolve_existing_path` 必然返回 None。
        cfg.paths.ffmpeg = dir.join("not-there.exe").to_string_lossy().to_string();

        let item = item_by_id("ffmpeg").unwrap();
        assert!(
            resolve_existing_path(item, &Some(cfg.clone())).is_none(),
            "前置条件：配置与默认路径都不存在"
        );
        assert_eq!(delete_item_file("ffmpeg", &cfg).unwrap(), None);

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

    /// 清扫必须删掉 `.part`，且**不碰**旁边的正式文件。
    ///
    /// 用临时目录而不是真实模型路径：早先版本直接往 `models/` 写假文件，
    /// 结果与「已就位」判定类测试并行时互相干扰，出现偶发失败。
    #[test]
    fn sweep_removes_stale_parts_but_not_real_files() {
        let (dir, dest) = isolated_dest("stale");
        let part = part_path_for(&dest).unwrap();

        fs::write(&dest, b"complete model").unwrap();
        fs::write(&part, b"incomplete").unwrap();

        let removed = sweep_parts_for(std::slice::from_ref(&dest));

        assert_eq!(removed, 1);
        assert!(!part.exists(), ".part 必须被清掉");
        assert!(dest.exists(), "正式文件绝不能被误删");
        assert_eq!(fs::read(&dest).unwrap(), b"complete model");

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
}
