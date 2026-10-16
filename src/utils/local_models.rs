//! 「本地有哪些语音识别模型」的扫描与归类。
//!
//! # 为什么需要这个模块
//!
//! 原先「用户能用哪个模型」这件事由 `WhisperModelTier` 这个 8 值枚举表达：
//! 抽屉引擎卡与性能页下拉都只在枚举里筛「已下载」的档位。于是用户自己放进
//! `models/whisper/` 的模型（自行量化的 q5_1、从别处下的 ggml 文件、改名后的
//! 权重）**在界面上根本不存在**——看不到、选不了、也不会被加载。
//!
//! 本模块把「有哪些模型」改成**扫盘事实**：目录里有什么就认什么。枚举退居为
//! 「已知预设的元数据」（友好名称、耗时估算），不再充当可用性判据。
//!
//! # 为什么不复用 `model_download::ITEMS`
//!
//! `ITEMS` 是**可下载清单**（19 条固定条目 + 每条的下载 URL / 体积 / 哈希），
//! 它的职责是「我们替用户下载什么」。而本模块回答的是「磁盘上现在有什么」——
//! 两者在「用户自备文件」这一整类上完全不重叠。用 `ITEMS` 做扫描会漏掉它们，
//! 这正是原实现的病根。

use std::path::{Path, PathBuf};

/// 扫描结果里的一条模型：一个**真实存在**的权重文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalModel {
    /// 权重文件的绝对路径。这是本项的唯一标识（不再用枚举当键）。
    pub path: PathBuf,
    /// 展示名（已知预设用友好名，未知文件用文件名）。
    pub label: String,
    /// 该文件命中的已知预设档位；`None` = 用户自备 / 未被预设登记。
    pub tier: Option<crate::app::WhisperModelTier>,
    /// 文件大小（字节）。用于展示与「用时估算」的降级启发式。
    pub size_bytes: u64,
}

impl LocalModel {
    /// 是否是我们认识的标准档位。
    pub fn is_known_tier(&self) -> bool {
        self.tier.is_some()
    }
}

/// 目标模型的扩展名白名单。
///
/// 只收 whisper.cpp 真正能加载的两种：`ggml-*.bin` 与 `.gguf`。
/// 刻意不按文件名前缀限制（不要求 `ggml-`）：用户把权重改名成 `my-model.bin`
/// 也应当被认出来——「什么名字都能用」正是本次要支持的场景。
const MODEL_EXTS: [&str; 2] = ["bin", "gguf"];

/// 判定某个文件名是否是**非 ASR 的伴生权重**，从而排除出模型列表。
///
/// 目前只有一类：Silero VAD（`ggml-silero-*.bin`）。它同样躺在
/// `models/whisper/` 目录里、同样是 `.bin`，但它不是识别模型——列进档位下拉
/// 会让用户选中后转写直接失败。按子串而非精确名判定：VAD 的文件名带版本号
/// （`ggml-silero-v6.2.0.bin`），精确匹配会在升级 VAD 时失效。
pub fn is_companion_weight(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower.contains("silero") || lower.contains("vad")
}

/// 该扩展名是否是候选模型文件。
pub fn is_model_extension(ext: &str) -> bool {
    MODEL_EXTS.iter().any(|want| ext.eq_ignore_ascii_case(want))
}

/// 扫描一个目录，返回其中所有可用的 ASR 模型（按名称排序，保证界面顺序稳定）。
///
/// # 为什么不在这里排序「预设优先」
///
/// 排序交给调用方（界面）决定：这里只保证**确定性**（同一目录两次扫描结果一致），
/// 否则每一帧的列表顺序都可能抖动。目录不存在时返回空表而不是错误——「还没下过
/// 任何模型」是正常的首次启动状态，不是故障。
pub fn scan_model_dir(dir: &Path) -> Vec<LocalModel> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<LocalModel> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // 只认普通文件：子目录（例如用户按档位分子目录存放）不在此列，
        // 递归扫描会把 VAD / 分片等一并卷入，代价大于收益。
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_model_extension(ext) || is_companion_weight(name) {
            continue;
        }
        found.push(describe(&path, name, meta.len()));
    }
    // 排序规则（显式规定，不依赖字节序的偶然结果）：
    //   1. 已知预设档位在前，按枚举声明顺序 = 体积/能力递增（Tiny → LargeV3），
    //      与性能页下拉的历史顺序一致，用户不用重新建立肌肉记忆；
    //   2. 自备模型在后，按展示名排序（通常就是文件名字母序）。
    //
    // 不直接按 label 排序：那样 "Whisper Small-Q5" 的大写 W（0x57）会排在
    // "my-custom.bin" 的小写 m（0x65）之前——顺序看似「预设在前」，实则只是
    // ASCII 大小写巧合，一旦自备模型取名以大写字母开头就会打乱分组。
    found.sort_by(|a, b| {
        let rank = |m: &LocalModel| match m.tier {
            Some(t) => (0u8, t as u8),
            None => (1u8, 0u8),
        };
        rank(a).cmp(&rank(b)).then_with(|| a.label.cmp(&b.label))
    });
    found
}

/// 把一个文件路径包装成 `LocalModel`（补上友好名与预设档位）。
///
/// 抽成独立函数是为了让「已知预设」与「用户自备」两条路径共用同一套构造，
/// 并让单测能直接喂路径而不必真造目录。
pub fn describe(path: &Path, file_name: &str, size_bytes: u64) -> LocalModel {
    let tier = crate::app::WhisperModelTier::from_model_path(file_name);
    let label = match tier {
        // 命中预设：用友好名（「Whisper Small-Q5」比 ggml-small-q5_0.bin 好认）
        Some(t) => t.label().to_string(),
        // 未命中：如实显示文件名，让用户认出自己放进去的是哪个
        None => file_name.to_string(),
    };
    LocalModel {
        path: path.to_path_buf(),
        label,
        tier,
        size_bytes,
    }
}

/// 本地 Whisper 权重的存放目录（绝对路径）。
///
/// 与 `model_download::ITEMS` 里各条目的 `dest` 前缀保持一致：下载器把权重都
/// 落在 `models/whisper/`。用户自备模型也应当放这里——扫描只看这个目录，
/// 放到别处不会被列出（但可以通过 `paths.whisper_model` 直接指向，见下）。
pub fn models_dir() -> PathBuf {
    crate::utils::AppConfig::resolve_path("models/whisper")
}

/// 扫描默认模型目录，得到「本机现在能用的全部 Whisper 模型」。
pub fn scan_installed_models() -> Vec<LocalModel> {
    scan_model_dir(&models_dir())
}

/// 本地大模型（GGUF）的存放目录（绝对路径）。
///
/// 与 `model_download::ITEMS` 里 `qwen-llm` / `qwen-llm-1_5b` / `qwen-llm-3b`
/// 等条目的 `dest` 前缀一致。
pub fn llm_models_dir() -> PathBuf {
    crate::utils::AppConfig::resolve_path("models/llm")
}

/// 扫描 `models/llm/`，返回本机已下载的全部离线大模型。
///
/// # 与 Whisper 扫描的区别
///
/// 这里**不做档位映射**（`tier` 恒为 `None`）：档位是 ASR 的概念，翻译/润色模型
/// 只有「叫什么名字、多大」两件事。界面据此把用户已下载的每个模型都列出来供点选，
/// 让他不必从文件名手抄一长串路径到输入框里。
///
/// 目录不存在返回空表（还没下过任何大模型的正常状态）。
pub fn scan_llm_models() -> Vec<LocalModel> {
    scan_gguf_dir(&llm_models_dir())
}

/// 扫描任意目录里的 GGUF 大模型（[`scan_llm_models`] 的可测内核）。
///
/// 抽成「喂目录」的形式而不是直接读 `models/llm`：否则单测只能依赖开发机
/// 实际下载过什么，无法验证「排除 .part / 只认 .gguf」这些关键过滤。
pub fn scan_gguf_dir(dir: &Path) -> Vec<LocalModel> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<LocalModel> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // 只认 .gguf：离线大模型统一是 GGUF 格式，非普通文件（目录）已在上一步挡掉。
        // 下载中的临时名是 `xxx.gguf.part`（扩展名 part），因此这一条同时排除了
        // 「半截模型」——它被列出来会让用户点选后加载失败。
        if !name.to_ascii_lowercase().ends_with(".gguf") {
            continue;
        }
        found.push(LocalModel {
            path: path.clone(),
            label: name.to_string(),
            tier: None,
            size_bytes: meta.len(),
        });
    }
    // 按名字排序：同一目录两次扫描顺序一致，界面不会每帧抖动。
    found.sort_by(|a, b| a.label.cmp(&b.label));
    found
}

/// 把任意绝对路径转成适合写进 `config.toml` 的路径字符串。
///
/// # 为什么必须走这一步
///
/// 配置层的约定是「**绝对路径即机器专属**」：`split_machine_paths` 会把绝对路径
/// 从共享的 `config.toml` 剥离到本机的 `config.local.toml`。项目目录内的模型
/// 若能写成相对路径，就能留在可提交的共享配置里——多个用户共享同一份配置时不会
/// 各自把对方的本机路径覆盖掉。
///
/// 目录外的自备模型（如 `D:/models/xx.bin`）如实保留绝对路径，由配置层剥离。
/// 分隔符统一成正斜杠：TOML 里反斜杠需要转义，写进去会变成难以手工编辑的形式。
pub fn to_config_path(path: &Path) -> String {
    let root = crate::utils::AppConfig::app_root_dir();
    // 能相对化就相对化；strip_prefix 失败（不同盘 / 不同根）时退回绝对路径。
    let p = path.strip_prefix(&root).unwrap_or(path);
    p.to_string_lossy().replace('\\', "/")
}

/// 未知模型的**体积启发式**档位：只用于「预计耗时」这类估算，不参与加载决策。
///
/// 分档依据是各档实测文件体积的中位区间（见 `model_download::ITEMS` 的 `size`）。
/// 之所以要有一个估算值：界面上的「预计 X 分钟」与 CPU/GPU 系数都挂在档位上，
/// 用户自备的 q5_1 若没有档位就会显示不出预计时间——而自备量化恰恰是本次要支持的
/// 主场景。宁可给一个粗略但可用的数字，也不要留空。
pub fn tier_for_unknown_size(size_bytes: u64) -> crate::app::WhisperModelTier {
    use crate::app::WhisperModelTier as T;
    const MB: u64 = 1024 * 1024;
    match size_bytes / MB {
        0..=110 => T::Fast,         // ≈ base(141MB) / tiny(75MB)
        111..=300 => T::Balanced,   // ≈ small-q5(167MB)
        301..=700 => T::TurboSpeed, // ≈ turbo-q5(547MB)
        701..=1200 => T::Precise,   // ≈ turbo-q8(833MB)
        1201..=2500 => T::Medium,   // ≈ medium(1.5GB)
        _ => T::LargeV3,            // ≈ large-v3(3.1GB)
    }
}

impl LocalModel {
    /// 用于耗时估算的档位：命中预设时就是它，否则按体积启发式。
    pub fn estimate_tier(&self) -> crate::app::WhisperModelTier {
        self.tier
            .unwrap_or_else(|| tier_for_unknown_size(self.size_bytes))
    }
}

/// 两个模型路径是否指向同一个文件。
///
/// 按 Windows 语义做大小写折叠（本项目是 Windows 桌面应用，NTFS 大小写不敏感）：
/// 配置里手写的 `ggml-Small-Q5_0.bin` 与扫描得到的实际大小写必须判定为同一项，
/// 否则界面会把「当前正在用的模型」显示成未选中。
///
/// 分隔符一并归一（`\` → `/`）：配置里可能写成反斜杠，而扫描结果来自
/// `PathBuf::join`，两者直接比较会不等。
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let norm = |p: &Path| p.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_of(m: &LocalModel) -> String {
        m.path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string()
    }

    /// 目录不存在 → 空表，不报错（首次启动的正常状态）。
    #[test]
    fn missing_dir_yields_empty_not_error() {
        let got = scan_model_dir(Path::new("Z:/definitely/not/here"));
        assert!(got.is_empty());
    }

    /// VAD 与其它伴生权重必须被排除：它们同目录同扩展名，但不是识别模型。
    #[test]
    fn companion_weights_are_excluded() {
        assert!(is_companion_weight("ggml-silero-v6.2.0.bin"));
        assert!(is_companion_weight("silero_vad.onnx"));
        assert!(is_companion_weight("SILERO-v5.bin"), "大小写不敏感");
        assert!(!is_companion_weight("ggml-small-q5_0.bin"));
        assert!(!is_companion_weight("ggml-large-v3-turbo-q5_0.bin"));
    }

    /// 只认 .bin / .gguf，且大小写不敏感。
    #[test]
    fn only_model_extensions_are_accepted() {
        assert!(is_model_extension("bin"));
        assert!(is_model_extension("BIN"));
        assert!(is_model_extension("gguf"));
        assert!(
            !is_model_extension("onnx"),
            "ONNX 归 SenseVoice，不经此路径"
        );
        assert!(!is_model_extension("txt"));
        assert!(!is_model_extension("json"));
    }

    /// 已知档位命中友好名；未知文件如实用文件名（本次要支持的核心场景）。
    #[test]
    fn known_tier_gets_friendly_label_and_unknown_keeps_file_name() {
        let known = describe(Path::new("m/ggml-small-q5_0.bin"), "ggml-small-q5_0.bin", 1);
        assert_eq!(known.tier, Some(crate::app::WhisperModelTier::Balanced));
        assert_eq!(known.label, "Whisper Small-Q5");

        let custom = describe(Path::new("m/my-custom-q5_1.bin"), "my-custom-q5_1.bin", 1);
        assert_eq!(custom.tier, None, "自备模型没有预设档位");
        assert_eq!(custom.label, "my-custom-q5_1.bin", "如实显示文件名");
    }

    /// 端到端：真造一个目录，确认「有什么列什么 + 排序稳定 + 排除 VAD」。
    #[test]
    fn scan_lists_every_model_and_is_deterministic() {
        let dir = std::env::temp_dir().join(format!("v2w_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");

        for f in [
            "ggml-small-q5_0.bin",
            "my-custom.bin",
            "extra.gguf",
            "ggml-silero-v6.2.0.bin", // 应被排除
            "notes.txt",              // 应被排除
        ] {
            std::fs::write(dir.join(f), b"x").expect("写测试文件");
        }
        std::fs::create_dir_all(dir.join("subdir")).expect("建子目录");
        std::fs::write(dir.join("subdir").join("nested.bin"), b"x").expect("写嵌套文件");

        let got = scan_model_dir(&dir);
        let names: Vec<String> = got.iter().map(name_of).collect();
        // 排序：已知预设在前（Whisper Small-Q5 → ggml-small-q5_0.bin），
        // 自备模型在后按名排序（extra.gguf、my-custom.bin）。
        assert_eq!(
            names,
            vec!["ggml-small-q5_0.bin", "extra.gguf", "my-custom.bin"],
            "预设在前、自备在后；排除 VAD/txt/子目录"
        );
        // 同一目录扫两次结果一致（渲染每帧重建列表，顺序抖动会让界面闪）
        assert_eq!(scan_model_dir(&dir), got, "扫描必须确定性");

        let custom = got.iter().find(|m| name_of(m) == "my-custom.bin").unwrap();
        assert!(custom.tier.is_none());
        assert!(!custom.is_known_tier());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 大模型扫描：只认 `.gguf`，跳过下载中的 `.part`，按名排序且确定性。
    ///
    /// 这几条锁住的是「多显示几个大模型」能真正可用：列出来的每一项都必须是
    /// **完整可用**的模型（半截的 `.part` 混进去，用户点选后会加载失败）。
    #[test]
    fn llm_scan_accepts_only_complete_gguf_files() {
        let dir = std::env::temp_dir().join(format!("v2w_llm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");

        for f in [
            "qwen2.5-1.5b-instruct-q4_k_m.gguf",
            "qwen2.5-0.5b-instruct-q4_k_m.gguf",
            "qwen2.5-3b-instruct-q4_k_m.gguf.part", // 下载中：必须排除
            "readme.txt",                           // 非模型：必须排除
            "sensevoice.onnx",                      // ONNX 不是 GGUF：必须排除
        ] {
            std::fs::write(dir.join(f), b"x").expect("写测试文件");
        }

        // 注意：这条测试直接调 `scan_llm_models` 的同构实现（喂目录），
        // 而不是 `scan_llm_models()`——后者固定读项目内的 models/llm，
        // 测它会依赖开发机的实际文件，不稳定。
        let got = scan_gguf_dir(&dir);
        let names: Vec<String> = got.iter().map(name_of).collect();
        assert_eq!(
            names,
            vec![
                "qwen2.5-0.5b-instruct-q4_k_m.gguf",
                "qwen2.5-1.5b-instruct-q4_k_m.gguf"
            ],
            "应只列完整 .gguf，且按名排序"
        );
        assert_eq!(scan_gguf_dir(&dir), got, "扫描必须确定性");
        // 大模型没有 ASR 档位概念：全部为 None，避免界面误显示 Whisper 档位名
        assert!(got.iter().all(|m| m.tier.is_none()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
