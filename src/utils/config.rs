//! 配置管理

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub paths: PathsConfig,
    pub pipeline: PipelineConfig,
    #[serde(default)]
    pub gpu: GpuConfig,
    /// 字幕全局样式与排版（主界面可调，实时预览并持久化）
    #[serde(default)]
    pub subtitle_style: SubtitleStyleConfig,
    /// 字幕翻译引擎（离线 Qwen / 在线 OpenAI 兼容 API）
    #[serde(default)]
    pub translate: TranslateConfig,
    /// 界面外观（深浅主题）
    #[serde(default)]
    pub ui: UiConfig,
}

/// 界面外观配置。主题是长期偏好，落盘避免每次启动都要重选。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiConfig {
    /// `dark`（默认） | `light`
    pub theme: String,
    /// 导出内容模式：`raw`（仅原文） | `translation`（仅译文） | `bilingual`（默认，双语）。
    ///
    /// 必须持久化：用户在导出栏选了「仅译文」后重启，若又跳回「双语」，
    /// 下次导出就会多出原文行——用户会以为选项没生效。
    #[serde(default = "default_export_mode")]
    pub export_mode: String,
}

fn default_export_mode() -> String {
    "bilingual".to_string()
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            export_mode: default_export_mode(),
        }
    }
}

impl UiConfig {
    pub fn is_light(&self) -> bool {
        self.theme.trim().eq_ignore_ascii_case("light")
    }

    /// 界面展示用的主题名
    pub fn label(&self) -> &'static str {
        if self.is_light() {
            "浅色"
        } else {
            "深色"
        }
    }

    pub fn toggled(&self) -> Self {
        Self {
            theme: if self.is_light() { "dark" } else { "light" }.to_string(),
            // 切换主题不应重置导出内容偏好
            export_mode: self.export_mode.clone(),
        }
    }
}

/// 字幕翻译引擎配置。
///
/// `mode` 决定走哪条链路：
/// - `offline_qwen`：本地 llama.cpp + Qwen 小模型，免费、断网可用、无需密钥；
/// - `online_api`：任何 OpenAI 兼容的 `/chat/completions` 接口
///   （DeepSeek、OpenAI、通义、Kimi、本地 vLLM / Ollama 均可），质量更高且快得多。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TranslateConfig {
    /// `offline_qwen`（默认） | `online_api`
    pub mode: String,
    /// 在线接口基址，不含 `/chat/completions`，例如 `https://api.deepseek.com/v1`
    pub api_base: String,
    /// Bearer 密钥。留空时会回退读取环境变量 `VOICE2WORD_API_KEY`，
    /// 避免把密钥明文写进 config.toml 后误提交。
    pub api_key: String,
    /// 模型名，例如 `deepseek-chat` / `gpt-4o-mini` / `qwen-plus`
    pub api_model: String,
    /// 单次请求携带的字幕条数。在线接口按 token 计费，批量越大往返次数越少。
    pub batch_size: usize,
    /// 请求超时（秒）。网络差时可调大。
    pub timeout_secs: u64,
    /// 上次选择的目标语言（与 `TRANSLATE_TARGET_LANGS` 同集合）。
    ///
    /// 必须持久化：目标语言是用户的长期偏好，重启后悄悄跳回「简体中文」
    /// 会让「上次译的是日语，这次怎么变中文了」无从解释。
    #[serde(default = "default_translate_target_lang")]
    pub target_lang: String,
}

fn default_translate_target_lang() -> String {
    "简体中文".to_string()
}

impl Default for TranslateConfig {
    fn default() -> Self {
        Self {
            mode: "offline_qwen".to_string(),
            api_base: "https://api.deepseek.com/v1".to_string(),
            api_key: String::new(),
            api_model: "deepseek-chat".to_string(),
            batch_size: 20,
            timeout_secs: 120,
            target_lang: default_translate_target_lang(),
        }
    }
}

impl TranslateConfig {
    /// 是否配置为在线 API 模式
    pub fn is_online(&self) -> bool {
        self.mode.eq_ignore_ascii_case("online_api")
    }

    /// 实际使用的密钥：配置优先，其次环境变量
    pub fn effective_api_key(&self) -> String {
        let key = self.api_key.trim();
        if !key.is_empty() {
            return key.to_string();
        }
        std::env::var("VOICE2WORD_API_KEY").unwrap_or_default()
    }

    /// 目标 URL：容错拼接，允许用户把基址写成带或不带尾部斜杠、甚至直接写到 /chat/completions
    pub fn chat_completions_url(&self) -> String {
        let base = self.api_base.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

/// 字幕全局样式与排版配置。
///
/// `font_size` / `letter_spacing` / `bottom_margin` 均以 **1080p 画面**为基准：
/// - 导出 ASS 时直接写入 PlayResY=1080 的样式表（1:1）；
/// - 预览时按 `PREVIEW_SCALE` 等比映射到监视器画面。
/// 这样「预览所见」与「导出所得」是同一套参数，不会各调各的。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubtitleStyleConfig {
    /// 字号 px @1080p
    pub font_size: u32,
    /// 字间距 px @1080p
    pub letter_spacing: u32,
    /// 行间距倍数
    pub line_spacing: f32,
    /// 单行最大字数（预览与导出按此宽度折行）
    pub max_chars_per_line: u32,
    /// 画面底边距 px @1080p
    pub bottom_margin: u32,
    /// 预设名称："白字黑影" / "黄字黑边" / "半透明黑框" / "电影沉浸"
    pub preset_name: String,
    /// 预览框的手动宽度（px）。
    ///
    /// `None`（旧配置 / 从未拖过）= 按「单行最大字数 × 预览字号」自动推算；
    /// `Some(w)` = 用户在预览条两侧拖过把手，以手动值为准。
    /// 用 `#[serde(default)]` 保证老 `config.toml` 缺这一项时能正常反序列化。
    #[serde(default)]
    pub preview_box_w: Option<f32>,
}

impl Default for SubtitleStyleConfig {
    fn default() -> Self {
        Self {
            font_size: 40,
            letter_spacing: 1,
            line_spacing: 1.2,
            max_chars_per_line: 16,
            bottom_margin: 40,
            preset_name: "白字黑影".to_string(),
            preview_box_w: None,
        }
    }
}

/// 可选预设列表（UI 与 `apply_preset` 共用同一份清单）
pub const SUBTITLE_PRESETS: [&str; 4] = ["白字黑影", "黄字黑边", "半透明黑框", "电影沉浸"];

impl SubtitleStyleConfig {
    /// 应用预设：预设不只是改个名字，而是同时套用一组排版参数，
    /// 否则「切换预设」对画面毫无影响，形同虚设。
    pub fn apply_preset(&mut self, preset: &str) {
        self.preset_name = preset.to_string();
        let (font_size, letter_spacing, line_spacing, max_chars, bottom_margin) = match preset {
            "黄字黑边" => (44, 1, 1.2, 16, 40),
            "半透明黑框" => (36, 1, 1.2, 16, 40),
            "电影沉浸" => (36, 4, 1.4, 20, 80),
            // 白字黑影（默认）
            _ => (40, 1, 1.2, 16, 40),
        };
        self.font_size = font_size;
        self.letter_spacing = letter_spacing;
        self.line_spacing = line_spacing;
        self.max_chars_per_line = max_chars;
        self.bottom_margin = bottom_margin;
    }

    /// 预览缩放系数：监视器画面高度约为 1080p 的 1/3，
    /// 把 1080p 基准字号换算成监视器上的可读字号（纯近似，导出仍按 1080p 原值）。
    pub const PREVIEW_SCALE: f32 = 0.32;

    /// 预览用字号（px）
    pub fn preview_font_px(&self) -> f32 {
        (self.font_size as f32 * Self::PREVIEW_SCALE).clamp(10.0, 26.0)
    }

    /// 预览用底边距（画面高度比例）
    pub fn preview_bottom_ratio(&self) -> f32 {
        (self.bottom_margin as f32 / 1080.0).clamp(0.0, 0.25)
    }
}

/// GPU 资源占用总闸：给桌面/其他应用留显卡
/// - hwaccel_decode: 预览播放/拖动的显卡硬解 (-hwaccel auto)，关闭后走 CPU 软解
/// - whisper_offload: 转写推理的 Vulkan GPU 加速 (whisper-vulkan)，关闭后走纯 CPU（转写变慢）
/// - yield_to_desktop: 「让路」总闸。转写/预览子进程以 BELOW_NORMAL_PRIORITY_CLASS 启动，
///   桌面合成器 (dwm) 与其它前台程序优先拿到 CPU 与 GPU 调度时间。
///
///   核显（如 Radeon 680M）既要算推理又要输出画面，Whisper Vulkan 会把 compute 队列
///   持续压到 65%~80%，此时整机拖窗、切窗口都会顿。实测（5 分钟音频 / small-q5_0 /
///   16 线程 / 680M）：
///     - 让路开：GPU 均值 62.7%、峰值 78.9%、耗时 18.4s
///     - 让路关：GPU 均值 65.1%、峰值 80.2%、耗时 19.7s
///   即让路几乎不损失速度，却把调度优先权还给桌面。设为 false 可换回满速抢占。
/// - gpu_limit_percent: GPU 占用上限（百分比，100 = 不限速）。
///   让路模式只降低进程调度优先级，管不到「已经排进 GPU 队列」的命令缓冲：
///   核显上 Whisper 依然会把 compute 队列压到 60%~80%，桌面照样掉帧。
///   设成小于 100 后，whisper-cli 会按占空比被挂起/恢复（跑 N% 时间、让出 (100-N)%），
///   给桌面合成器留出真正的 GPU 空窗。代价是总耗时约放大 100/N 倍。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuConfig {
    #[serde(default)]
    pub hwaccel_decode: bool,
    #[serde(default)]
    pub whisper_offload: bool,
    #[serde(default = "default_true")]
    pub yield_to_desktop: bool,
    #[serde(default = "default_gpu_limit_percent")]
    pub gpu_limit_percent: u32,
}

fn default_gpu_limit_percent() -> u32 {
    100
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            hwaccel_decode: false,
            whisper_offload: false,
            yield_to_desktop: true,
            gpu_limit_percent: 100,
        }
    }
}

impl GpuConfig {
    /// UI 四档策略名:
    /// - "full"     全速 GPU：硬解预览 + GPU 转写，子进程不降优先级（显卡吃满，桌面可能顿）
    /// - "balanced" GPU 让路：软解预览 + GPU 转写，子进程降到低于正常优先级，桌面优先
    /// - "eco"      低占用 GPU：在让路基础上再按占空比给 GPU 留白，核显占用降到设定上限
    /// - "cpu"      纯 CPU：完全不碰显卡，核显占用 0%，转写约慢一倍
    pub fn mode(&self) -> &'static str {
        match (self.hwaccel_decode, self.whisper_offload) {
            // 只有「GPU 转写 + 不让路」才算全速；只要让路就归入均衡档
            (true, true) if !self.yield_to_desktop => "full",
            // 限速档：GPU 仍参与推理，但按占空比主动留白
            (_, true) if self.gpu_limit_percent < 100 => "eco",
            (_, true) => "balanced",
            // 可保留视频硬解，但 Whisper 明确走 CPU，归入纯 CPU 推理档
            _ => "cpu",
        }
    }

    pub fn from_mode(mode: &str) -> Self {
        match mode {
            "full" => Self {
                hwaccel_decode: true,
                whisper_offload: true,
                yield_to_desktop: false,
                gpu_limit_percent: 100,
            },
            "balanced" => Self {
                hwaccel_decode: false,
                whisper_offload: true,
                yield_to_desktop: true,
                gpu_limit_percent: 100,
            },
            "eco" => Self {
                hwaccel_decode: false,
                whisper_offload: true,
                yield_to_desktop: true,
                gpu_limit_percent: 60,
            },
            "cpu" => Self {
                hwaccel_decode: false,
                whisper_offload: false,
                yield_to_desktop: true,
                gpu_limit_percent: 100,
            },
            _ => Self::default(),
        }
    }

    /// 归一化后的 GPU 占用上限：100 表示不限速；其余夹在 20~95，
    /// 低于 20% 时每段有效计算不足一个解码步，速度会塌方，不如直接选纯 CPU。
    pub fn effective_gpu_limit(&self) -> u32 {
        if self.gpu_limit_percent >= 100 {
            100
        } else {
            self.gpu_limit_percent.clamp(20, 95)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsConfig {
    pub ffmpeg: String,
    pub whisper_cli: String,
    pub whisper_model: String,
    #[serde(default)]
    pub vad_model: Option<String>,
    #[serde(default = "default_punc_model")]
    pub punc_model: Option<String>,
    #[serde(default = "default_sensevoice_model")]
    pub sensevoice_model: Option<String>,
    #[serde(default = "default_sensevoice_tokens")]
    pub sensevoice_tokens: Option<String>,
    #[serde(default = "default_sensevoice_vad")]
    pub sensevoice_vad: Option<String>,
    pub llama_cli: String,
    pub llm_model: String,
    /// 运行 SenseVoice / CT-Punc 的 Python 脚本所用的解释器。
    /// 默认走系统 PATH 的 `python`；若机器上有多个 Python（如 conda、微软商店版），
    /// 可在此指定绝对路径，避免解析到缺少 numpy / sherpa_onnx 的解释器。
    #[serde(default = "default_python")]
    pub python: String,
}

impl Default for PathsConfig {
    /// 便携默认路径：全部相对项目根，可安全提交。
    /// 本机若把 ffmpeg / llama.cpp 装在别处，请写 config.local.toml 覆盖。
    fn default() -> Self {
        Self {
            ffmpeg: "tools/ffmpeg.exe".to_string(),
            whisper_cli: "tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe".to_string(),
            whisper_model: "models/whisper/ggml-small-q5_0.bin".to_string(),
            vad_model: Some("models/whisper/ggml-silero-v6.2.0.bin".to_string()),
            punc_model: Some("models/punc/model.int8.onnx".to_string()),
            sensevoice_model: Some("models/sensevoice/model.int8.onnx".to_string()),
            sensevoice_tokens: Some("models/sensevoice/tokens.txt".to_string()),
            sensevoice_vad: Some("models/sensevoice/silero_vad.onnx".to_string()),
            llama_cli: "tools/llama-completion.exe".to_string(),
            llm_model: "models/llm/qwen2.5-0.5b-instruct-q4_k_m.gguf".to_string(),
            python: default_python(),
        }
    }
}

fn default_python() -> String {
    "python".to_string()
}

fn default_true() -> bool {
    true
}

fn default_punc_model() -> Option<String> {
    Some("models/punc/model.int8.onnx".to_string())
}

fn default_sensevoice_model() -> Option<String> {
    Some("models/sensevoice/model.int8.onnx".to_string())
}

fn default_sensevoice_tokens() -> Option<String> {
    Some("models/sensevoice/tokens.txt".to_string())
}

fn default_sensevoice_vad() -> Option<String> {
    Some("models/sensevoice/silero_vad.onnx".to_string())
}

fn default_polish_mode() -> String {
    "punc".to_string()
}

fn default_rescue_logprob() -> f64 {
    // 默认关闭：二段救场每个窗口都是一次 FFmpeg seek + whisper-cli 全进程冷启动
    // （大模型 load + Vulkan 初始化数秒起），顺序执行极易把转写总耗时拉长
    // 30%~100%。需要质量兜底的用户可在 config.toml 中设为负值（如 -0.65）手动开启。
    0.0
}

fn default_max_context() -> u32 {
    32
}

fn default_audio_speed() -> f64 {
    1.0
}

fn default_vad_threshold() -> f64 {
    0.50
}

fn default_speaker_count() -> u32 {
    2
}

fn default_highpass_hz() -> f64 {
    70.0
}

fn default_min_saving() -> f64 {
    0.10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    pub language: String,
    pub output_format: String,
    pub enable_polish: bool,
    /// 润色模式："punc" (CT-Punc 极速标点, 默认) | "qwen" (大模型润色) | "none" (关闭)
    #[serde(default = "default_polish_mode")]
    pub polish_mode: String,
    #[serde(default = "default_true")]
    pub enable_vad: bool,
    pub whisper_threads: u32,
    /// 0 表示按 CPU 核数自动推导；GPU 后端应设置为 1，避免争用单个设备。
    #[serde(default)]
    pub whisper_processors: u32,
    /// 是否对 turbo 等大模型同样禁用温度回退 (-nf) 以提速；
    /// 低置信片段可由置信度二段重解码救回（whisper_rescue_logprob < 0 时手动开启，默认关闭）
    #[serde(default = "default_true")]
    pub whisper_no_fallback: bool,
    /// 置信度救场阈值：avg_logprob 低于该值的片段触发带回退重解码；
    /// 0.0 = 关闭救场（默认，救场是顺序冷启动进程，开销远大于收益）
    #[serde(default = "default_rescue_logprob")]
    pub whisper_rescue_logprob: f64,
    /// 跨句自注意力上下文 token 上限 (-mc，原硬编码 32，暴露出来供 A/B 实验)
    #[serde(default = "default_max_context")]
    pub whisper_max_context: u32,
    /// 只加速 Whisper 输入音频；字幕时间戳随后映射回原视频时间轴。
    #[serde(default = "default_audio_speed")]
    pub whisper_audio_speed: f64,
    /// Silero VAD 语音阈值；越高越激进，轻声越可能被跳过。
    #[serde(default = "default_vad_threshold")]
    pub whisper_vad_threshold: f64,
    /// 音频前端预处理总开关（仅 Whisper 路径）：语音增强 + 停顿压实。
    ///
    /// 关掉后完全回到「原始音频直喂模型」的旧行为，便于 A/B 对比。
    #[serde(default = "default_true")]
    pub preprocess_enabled: bool,
    /// FFT 谱减降噪：削掉稳态底噪，避免 Whisper 在静音处「听出」词语。
    #[serde(default = "default_true")]
    pub preprocess_denoise: bool,
    /// 语音电平动态归一：远场/手机录音电平偏低，归一后 VAD 阈值与
    /// `no_speech` 判定才可靠，能显著减少碎段（每段都要重付一轮解码）。
    #[serde(default = "default_true")]
    pub preprocess_normalize: bool,
    /// 高通截止频率（Hz），0 = 关闭。Whisper 的 mel 从 80 Hz 起算，
    /// 低于此的频率只贡献能量不贡献信息。
    #[serde(default = "default_highpass_hz")]
    pub preprocess_highpass_hz: f64,
    /// 停顿压实：把句间长静音整段切除后再喂模型。
    ///
    /// **默认关闭**。原因是实测它与 whisper.cpp 自带的 Silero VAD
    /// （`--vad`，只要配了 `vad_model` 就恒定开启）功能重叠：内置 VAD 已经
    /// 切掉静音并把时间戳映射回原轴，再在外面压一遍不会减少模型工作量，
    /// 却会多付一次 FFmpeg 整轨解码、多写一个临时 WAV，并因「每个 VAD 切片
    /// 各自向上取整到 30 秒窗口」而**增加**编码器窗口数。
    ///
    /// 同一 10 分钟片段的确定性对比（编码窗口数 / 解码 token 数）：
    /// 原始 600 s 音频 = 18 窗口 / 3242 token；外置压实后 538.8 s = 18 窗口 / 3218 token。
    /// 端到端也印证了这一点：开启压实的一轮 ASR 耗时 112.26 s，关闭后 70.17 s。
    /// `docs/Whisper人工标准字幕对比.md` 中「前置 VAD 压缩实验没有接入主流程」
    /// 的结论与此一致。
    ///
    /// 需要留作实验开关时把它设为 true 即可，此时管线会放弃纯内存推流、
    /// 改走临时 WAV 通道以支持随机访问压实。
    #[serde(default)]
    pub preprocess_compact: bool,
    /// 最小收益阈值：可切除静音占比低于该值时放弃压实。
    /// 压实要多写一个临时 WAV，收益太小时不划算。
    #[serde(default = "default_min_saving")]
    pub preprocess_min_saving: f64,
    pub llm_threads: u32,
    pub llm_ctx: u32,
    /// 并行进程数：`0` = 自动（按 CPU 核数推导引擎内实测调优值）；`>0` = 显式指定。
    /// 仅对长音频的多进程切块并行生效（Whisper 与 SenseVoice 共用同一开关）。
    #[serde(default)]
    pub parallel_workers: u32,
    /// 说话人分离（F-015）：转写完成后按声学特征聚类，为每句打上「说话人 N」标签。
    /// 纯 CPU 低阶特征实现，不需要额外的声纹模型。
    #[serde(default)]
    pub enable_diarization: bool,
    /// 期望说话人数（2~4）。聚类用确定性 k-means，该值直接决定簇数。
    #[serde(default = "default_speaker_count")]
    pub speaker_count: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            // 默认值必须是**便携**的（相对项目根），否则「没有 config.toml 时
            // 生成的默认配置」会把开发机的绝对路径写进去，再被提交。
            // 本机实际路径请写进 config.local.toml（见 LocalOverride）。
            paths: PathsConfig {
                ffmpeg: "tools/ffmpeg.exe".to_string(),
                whisper_cli: "tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe"
                    .to_string(),
                whisper_model: "models/whisper/ggml-small-q5_0.bin".to_string(),
                vad_model: Some("models/whisper/ggml-silero-v6.2.0.bin".to_string()),
                punc_model: Some("models/punc/model.int8.onnx".to_string()),
                sensevoice_model: Some("models/sensevoice/model.int8.onnx".to_string()),
                sensevoice_tokens: Some("models/sensevoice/tokens.txt".to_string()),
                sensevoice_vad: Some("models/sensevoice/silero_vad.onnx".to_string()),
                llama_cli: "tools/llama-completion.exe"
                    .to_string(),
                llm_model: "models/llm/qwen2.5-0.5b-instruct-q4_k_m.gguf".to_string(),
                python: "python".to_string(),
            },
            pipeline: PipelineConfig {
                language: "zh".to_string(),
                output_format: "srt".to_string(),
                enable_polish: false,
                polish_mode: "punc".to_string(),
                enable_vad: true,
                whisper_threads: 16,
                whisper_processors: 1,
                whisper_no_fallback: true,
                whisper_rescue_logprob: 0.0,
                whisper_max_context: 32,
                whisper_audio_speed: 1.0,
                whisper_vad_threshold: 0.50,
                preprocess_enabled: true,
                preprocess_denoise: true,
                preprocess_normalize: true,
                preprocess_highpass_hz: 70.0,
                preprocess_compact: false,
                preprocess_min_saving: 0.10,
                llm_threads: 8,
                llm_ctx: 4096,
                parallel_workers: 0,
                enable_diarization: false,
                speaker_count: 2,
            },
            gpu: GpuConfig::default(),
            subtitle_style: SubtitleStyleConfig::default(),
            translate: TranslateConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

/// 机器本地覆盖文件：`config.local.toml`（不纳入版本控制）。
///
/// 每个字段都是 `Option`：**只有写了的字段才会覆盖**，没写的沿用 `config.toml`。
/// 因此本文件通常只有寥寥几行，例如：
///
/// ```toml
/// [paths]
/// ffmpeg    = 'A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe'
/// llama_cli = 'A:\cppsoft\llama.cpp\build\bin\Release\llama-completion.exe'
/// ```
///
/// 设计上只承载「路径」与「GPU 策略」这两类真正因机器而异的东西；
/// 调参（线程、VAD、润色模式、字幕样式）留在共享配置里，
/// 免得同一份实验参数被拆到两个文件、A/B 时漏改一处。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalOverride {
    pub paths: Option<LocalPathsOverride>,
    pub gpu: Option<GpuConfig>,
}

/// `[paths]` 的逐字段覆盖。全部为 `Option`，未写的保持共享配置的值。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalPathsOverride {
    pub ffmpeg: Option<String>,
    pub whisper_cli: Option<String>,
    pub whisper_model: Option<String>,
    pub vad_model: Option<String>,
    pub punc_model: Option<String>,
    pub sensevoice_model: Option<String>,
    pub sensevoice_tokens: Option<String>,
    pub sensevoice_vad: Option<String>,
    pub llama_cli: Option<String>,
    pub llm_model: Option<String>,
    pub python: Option<String>,
}

impl LocalPathsOverride {
    /// 是否记录到任何一项（全空则不必落盘覆盖文件）。
    pub fn has_any(&self) -> bool {
        self.ffmpeg.is_some()
            || self.whisper_cli.is_some()
            || self.whisper_model.is_some()
            || self.vad_model.is_some()
            || self.punc_model.is_some()
            || self.sensevoice_model.is_some()
            || self.sensevoice_tokens.is_some()
            || self.sensevoice_vad.is_some()
            || self.llama_cli.is_some()
            || self.llm_model.is_some()
            || self.python.is_some()
    }
}

impl LocalOverride {
    /// 把覆盖应用到共享配置上，返回被覆盖的段名（用于启动日志）。
    pub fn apply_to(&self, cfg: &mut AppConfig) -> Vec<&'static str> {
        let mut touched = Vec::new();

        if let Some(p) = self.paths.as_ref() {
            // 宏化这组「有值才覆盖」的赋值，避免 11 行手写 if-let 里漏掉一个字段。
            macro_rules! apply_path {
                ($($field:ident),* $(,)?) => {
                    $(
                        if let Some(v) = p.$field.as_ref() {
                            cfg.paths.$field = Some(v.clone());
                        }
                    )*
                };
            }
            // 三个必填字段是 String，其余是 Option<String>，分开处理
            if let Some(v) = p.ffmpeg.as_ref() {
                cfg.paths.ffmpeg = v.clone();
            }
            if let Some(v) = p.whisper_cli.as_ref() {
                cfg.paths.whisper_cli = v.clone();
            }
            if let Some(v) = p.whisper_model.as_ref() {
                cfg.paths.whisper_model = v.clone();
            }
            if let Some(v) = p.llama_cli.as_ref() {
                cfg.paths.llama_cli = v.clone();
            }
            if let Some(v) = p.llm_model.as_ref() {
                cfg.paths.llm_model = v.clone();
            }
            if let Some(v) = p.python.as_ref() {
                cfg.paths.python = v.clone();
            }
            apply_path!(vad_model, punc_model, sensevoice_model, sensevoice_tokens, sensevoice_vad);
            touched.push("paths");
        }

        if let Some(g) = self.gpu.as_ref() {
            cfg.gpu = g.clone();
            touched.push("gpu");
        }

        touched
    }
}
impl AppConfig {
    /// 机器本地覆盖文件名（与 config.toml 同目录，不进版本库）。
    pub const LOCAL_OVERRIDE: &'static str = "config.local.toml";

    /// 读取配置，并在其后叠加机器本地覆盖文件 `config.local.toml`。
    ///
    /// # 为什么要分层
    ///
    /// `config.toml` 是**被 git 跟踪**的共享配置，但它里面混着两类东西：
    /// 1. 所有人都一样的调参（线程数、VAD 阈值、字幕样式、润色模式…）；
    /// 2. **只对本机成立**的绝对路径，例如 `A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe`。
    ///
    /// 第 2 类一旦提交，别人 clone 下来直接就是「你的机器」——路径不存在，
    /// 启动即报错，且每个人都要先把这几行改掉才能跑，改完又会把改动推回去。
    ///
    /// 分层后：`config.toml` 只留可共享项与**相对路径默认值**；
    /// 机器专属项写进 `config.local.toml`（已加进 `.gitignore`），
    /// 在这里做一次字段级覆盖。缺失该文件是完全正常的情况（CI、新机器）。
    ///
    /// 覆盖是**逐字段**而非「整段替换」：本地文件里只写 `[paths] ffmpeg` 一项时，
    /// 其余路径仍取 `config.toml` 的值，不必把整段复制一遍。
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {

        let full_path = Self::resolve_path(path.as_ref().to_str().unwrap_or("config.toml"));
        let mut cfg = if full_path.exists() {
            let content = std::fs::read_to_string(&full_path)
                .with_context(|| format!("读取配置文件失败: {:?}", full_path))?;
            toml::from_str(&content).with_context(|| "反序列化 config.toml 失败")?
        } else {
            let default_cfg = Self::default();
            default_cfg.save_to_file(&full_path)?;
            default_cfg
        };

        // 本地覆盖是可选的：不存在、或解析失败，都不应阻断启动——
        // 覆盖文件坏掉时退回共享配置，比整个应用起不来要好。
        //
        // 注意必须**紧邻 config.toml** 解析，不能走 resolve_path：后者以项目根为基准，
        // 会把覆盖文件错认到项目根下，于是「自定义路径的配置」永远读不到自己的覆盖。
        let local_path = Self::local_override_path(&full_path);
        if local_path.exists() {
            match std::fs::read_to_string(&local_path)
                .map_err(anyhow::Error::from)
                .and_then(|c| {
                    toml::from_str::<LocalOverride>(&c)
                        .map_err(anyhow::Error::from)
                        .with_context(|| format!("反序列化 {} 失败", Self::LOCAL_OVERRIDE))
                }) {
                Ok(local) => {
                    let touched = local.apply_to(&mut cfg);
                    if !touched.is_empty() {
                        tracing::info!(
                            file = Self::LOCAL_OVERRIDE,
                            sections = ?touched,
                            "已应用本机配置覆盖"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        file = Self::LOCAL_OVERRIDE,
                        error = %err,
                        "本机配置覆盖读取失败，已忽略并继续使用 config.toml"
                    );
                }
            }
        }
        Ok(cfg)
    }

    /// 配置路径参数的规范化：把机器专属的绝对路径拆到 `config.local.toml`，
    /// `config.toml` 只保留可提交的相对路径。
    ///
    /// 判定规则只有一条、且是客观的：**绝对路径即机器专属**。
    /// 相对路径（如 `models/whisper/ggml-small-q5_0.bin`）随项目根走，人人可共享。
    ///
    /// 这样即使 UI 触发的保存把完整配置序列化一遍，写进 `config.toml` 的
    /// 也永远是可提交的值——机器路径不会随一次「改主题」被悄悄提交回去。
    fn split_machine_paths(&self) -> (PathsConfig, LocalPathsOverride) {
        let portable = PathsConfig::default();
        let mut shared = self.paths.clone();
        let mut local = LocalPathsOverride::default();

        // 宏化 11 个字段的「绝对则剥离」逻辑，避免手写时漏掉某个字段。
        // 形参是「字段名」，两类字段（String / Option<String>）分别展开。
        macro_rules! strip_required {
            ($($f:ident),* $(,)?) => {
                $(
                    if Path::new(&self.paths.$f).is_absolute() {
                        local.$f = Some(self.paths.$f.clone());
                        shared.$f = portable.$f.clone();
                    }
                )*
            };
        }
        macro_rules! strip_optional {
            ($($f:ident),* $(,)?) => {
                $(
                    if let Some(v) = self.paths.$f.as_ref() {
                        if Path::new(v).is_absolute() {
                            local.$f = Some(v.clone());
                            shared.$f = portable.$f.clone();
                        }
                    }
                )*
            };
        }

        strip_required!(ffmpeg, whisper_cli, whisper_model, llama_cli, llm_model, python);
        strip_optional!(vad_model, punc_model, sensevoice_model, sensevoice_tokens, sensevoice_vad);

        (shared, local)
    }

    /// 与 `config.toml` 同目录的 `config.local.toml` 路径。
    fn local_override_path(config_path: &Path) -> PathBuf {
        config_path.with_file_name(Self::LOCAL_OVERRIDE)
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let full_path = Self::resolve_path(path.as_ref().to_str().unwrap_or("config.toml"));
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let (shared_paths, local_paths) = self.split_machine_paths();

        // 顺序很重要：**先落本机覆盖，再写共享配置**。
        // 共享配置里那几项会被换成便携默认值（如 `tools/ffmpeg.exe`），
        // 只有覆盖文件确实已经在磁盘上，换过去才是安全的；反过来先写共享配置，
        // 一旦覆盖文件写失败，用户的可用路径就丢了。
        let local_file = Self::local_override_path(&full_path);
        let should_write_local = local_paths.has_any();
        if should_write_local {
            let local = LocalOverride {
                paths: Some(local_paths),
                gpu: None,
            };
            let text =
                toml::to_string_pretty(&local).with_context(|| "序列化本机配置覆盖失败")?;
            let body = format!(
                "# 本机专属配置（由程序自动维护，不进版本库）\n\
                 #\n\
                 # 这里只存「换台机器就不成立」的东西：绝对路径、机器相关的 GPU 策略。\n\
                 # 共享调参请写 config.toml；本文件的字段会逐项覆盖它。\n\
                 # 删除本文件不影响启动，只是路径会回退到 config.toml 里的可移植默认值。\n\n{text}"
            );
            // 内容没变就不重复落盘：UI 上每敲一个键都会走到这里（见 commit_api_field），
            // 而这个文件几乎从不变，没必要每个字符都做一次磁盘写。
            let unchanged = std::fs::read_to_string(&local_file)
                .map(|existing| existing == body)
                .unwrap_or(false);
            if !unchanged {
                Self::atomic_write(&local_file, &body)
                    .with_context(|| format!("写回本机配置失败: {:?}", local_file))?;
            }
        }

        let mut on_disk = self.clone();
        on_disk.paths = shared_paths;
        let content =
            toml::to_string_pretty(&on_disk).with_context(|| "序列化配置为 TOML 失败")?;
        // 与本机覆盖同理：内容没变就不重复落盘。UI 上每敲一个键都会走到
        // 这里（见 `commit_api_field`），而在 API Key / 模型名里输入时共享配置并不改变——
        // 没必要每个字符都做一次写盘 + rename。
        let unchanged = std::fs::read_to_string(&full_path)
            .map(|existing| existing == content)
            .unwrap_or(false);
        if unchanged {
            return Ok(());
        }
        Self::atomic_write(&full_path, &content)
    }

    /// 原子写：先写同目录临时文件再 rename 覆盖。
    ///
    /// UI 上每敲一个键都会调用 `save_to_file`（见 `commit_api_field`），直接
    /// `fs::write` 会先把原文件截断为 0 字节；此时若进程被强杀或断电，
    /// 用户的 config.toml 就会变成空文件/半截文件而丢失全部设置。
    /// rename 在同一目录内是原子的，因此要么是旧内容、要么是新内容。
    fn atomic_write(path: &Path, content: &str) -> Result<()> {
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, content)?;
        if let Err(err) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("写回配置失败: {:?}", path));
        }
        Ok(())
    }

    /// 获取应用真实的根目录（优先查找当前目录、exe所在目录、或exe上上级目录）
    pub fn app_root_dir() -> PathBuf {
        // 1. 如果当前工作目录包含 models 目录，直接返回当前目录
        let cur = std::env::current_dir().unwrap_or_default();
        if cur.join("models").is_dir() {
            return cur;
        }

        // 2. 如果是通过 exe 启动，优先判断是否在 target/debug 或 target/release 下
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                // 如果是在 target/debug 或 target/release 下，向上寻找项目根目录
                let dir_name = exe_dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if dir_name.eq_ignore_ascii_case("debug")
                    || dir_name.eq_ignore_ascii_case("release")
                {
                    if let Some(target_dir) = exe_dir.parent() {
                        if target_dir.file_name().and_then(|s| s.to_str()) == Some("target") {
                            if let Some(root) = target_dir.parent() {
                                if root.join("models").is_dir() {
                                    return root.to_path_buf();
                                }
                            }
                        }
                    }
                }

                // 如果 exe 所在目录本身就包含 models
                if exe_dir.join("models").is_dir() {
                    return exe_dir.to_path_buf();
                }

                // 向上逐级寻找包含 models 的祖先目录
                let mut p = exe_dir.to_path_buf();
                for _ in 0..5 {
                    if p.join("models").is_dir() {
                        return p;
                    }
                    if let Some(parent) = p.parent() {
                        p = parent.to_path_buf();
                    } else {
                        break;
                    }
                }
            }
        }

        // 3. 兜底逐级向上查找包含 models 的目录
        let mut p = cur.clone();
        for _ in 0..5 {
            if p.join("models").is_dir() {
                return p;
            }
            if let Some(parent) = p.parent() {
                p = parent.to_path_buf();
            } else {
                break;
            }
        }

        cur
    }

    /// 解析相对路径为相对于项目根目录的绝对路径
    pub fn resolve_path(p: &str) -> PathBuf {
        let path = PathBuf::from(p);
        if path.is_absolute() {
            path
        } else {
            Self::app_root_dir().join(path)
        }
    }

    /// 解析外部命令路径：纯命令名（如 "python"）交给系统 PATH 查找，
    /// 含路径分隔符的才按项目根目录展开为绝对路径。
    ///
    /// 不能对 "python" 直接调用 resolve_path，否则会被拼成
    /// `<项目根>/python`，导致「找不到解释器」。
    pub fn resolve_command(cmd: &str) -> PathBuf {
        if cmd.contains('/') || cmd.contains('\\') {
            Self::resolve_path(cmd)
        } else {
            PathBuf::from(cmd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TranslateConfig, UiConfig};

    /// 切换主题不能顺手把导出内容偏好重置回默认「双语」。
    #[test]
    fn toggled_theme_preserves_export_mode() {
        let mut ui = UiConfig::default();
        ui.export_mode = "translation".to_string();
        let toggled = ui.toggled();
        assert!(toggled.is_light(), "默认深色切换后应为浅色");
        assert_eq!(toggled.export_mode, "translation", "切主题不应重置导出内容偏好");
        // 再切一次回深色，导出内容仍保留
        assert_eq!(toggled.toggled().export_mode, "translation");
    }

    /// 旧配置（没有 export_mode 字段）反序列化必须成功并落到默认「bilingual」。
    #[test]
    fn legacy_ui_config_defaults_export_mode_to_bilingual() {
        let parsed: UiConfig = toml::from_str("theme = \"dark\"\n").expect("旧配置应能解析");
        assert_eq!(parsed.export_mode, "bilingual");
    }

    #[test]
    fn chat_completions_url_tolerates_trailing_slash_and_full_path() {
        let mut cfg = TranslateConfig::default();
        cfg.api_base = "https://api.deepseek.com/v1".to_string();
        assert_eq!(
            cfg.chat_completions_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        cfg.api_base = "https://api.deepseek.com/v1/".to_string();
        assert_eq!(
            cfg.chat_completions_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        // 用户直接把完整端点填进来时不应再拼一层
        cfg.api_base = "https://api.openai.com/v1/chat/completions".to_string();
        assert_eq!(
            cfg.chat_completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn api_key_falls_back_to_env() {
        let cfg = TranslateConfig {
            api_key: "  ".to_string(),
            ..Default::default()
        };
        // 不依赖本机是否真的设置了该环境变量，只校验「空白配置不会被当成有效密钥」
        assert!(cfg.api_key.trim().is_empty());
        assert!(!cfg.is_online());
    }

    #[test]
    fn translate_section_roundtrips_through_toml() {
        let cfg = super::AppConfig::default();
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        let back: super::AppConfig = toml::from_str(&text).expect("deserialize");
        assert_eq!(back.translate, cfg.translate);
    }

    #[test]
    fn preprocess_compaction_is_off_by_default() {
        // 外置停顿压实与 whisper.cpp 内置的 Silero VAD（--vad）功能重叠：
        // 实测不减少编码窗口与解码 token，却要多写一个临时 WAV，
        // 并因为「每个 VAD 切片各自向上取整到 30 秒窗口」而增加编码器窗口数，
        // 所以默认必须是关闭的，否则会白白放弃纯内存推流通道。
        let cfg = super::AppConfig::default();
        assert!(!cfg.pipeline.preprocess_compact);
        assert!(cfg.pipeline.preprocess_enabled, "语音增强仍应默认开启");
        assert!(cfg.pipeline.preprocess_denoise);
        assert!(cfg.pipeline.preprocess_normalize);

        // 老配置文件里写了 true 仍要能被读进来（保持向后兼容）
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        let with_compact = text.replace("preprocess_compact = false", "preprocess_compact = true");
        let back: super::AppConfig = toml::from_str(&with_compact).expect("deserialize");
        assert!(back.pipeline.preprocess_compact);
    }

    // ─────────── 配置分层（config.toml + config.local.toml）───────────

    /// 本机覆盖必须逐字段生效，且**没写的字段沿用共享配置**。
    #[test]
    fn local_override_applies_field_by_field() {
        let mut cfg = super::AppConfig::default();
        cfg.paths.ffmpeg = "tools/ffmpeg.exe".to_string();
        cfg.paths.llama_cli = "tools/llama-completion.exe".to_string();
        cfg.paths.whisper_cli = "tools/whisper.exe".to_string();

        let local: super::LocalOverride = toml::from_str(
            r#"
            [paths]
            ffmpeg = 'D:\soft\ffmpeg.exe'
            "#,
        )
        .expect("parse local override");

        let touched = local.apply_to(&mut cfg);
        assert_eq!(touched, vec!["paths"]);
        assert_eq!(cfg.paths.ffmpeg, r"D:\soft\ffmpeg.exe");
        // 未在覆盖文件里出现的字段必须原样保留
        assert_eq!(cfg.paths.llama_cli, "tools/llama-completion.exe");
        assert_eq!(cfg.paths.whisper_cli, "tools/whisper.exe");
    }

    /// 覆盖文件里的 `[gpu]` 段整体替换 GPU 策略（这几项本就是机器相关的）。
    #[test]
    fn local_override_replaces_gpu_section() {
        let mut cfg = super::AppConfig::default();
        cfg.gpu.whisper_offload = false;

        let local: super::LocalOverride = toml::from_str(
            r#"
            [gpu]
            hwaccel_decode = false
            whisper_offload = true
            yield_to_desktop = true
            gpu_limit_percent = 60
            "#,
        )
        .expect("parse local override");

        let touched = local.apply_to(&mut cfg);
        assert_eq!(touched, vec!["gpu"]);
        assert!(cfg.gpu.whisper_offload);
        assert_eq!(cfg.gpu.mode(), "eco");
    }

    /// 空覆盖文件不应改动任何东西，也不该被报告为「已应用」。
    #[test]
    fn empty_local_override_is_a_noop() {
        let mut cfg = super::AppConfig::default();
        let before = cfg.paths.ffmpeg.clone();
        let local: super::LocalOverride = toml::from_str("").expect("empty is valid");
        assert!(local.apply_to(&mut cfg).is_empty());
        assert_eq!(cfg.paths.ffmpeg, before);
    }

    /// 绝对路径必须被剥离到本地覆盖，共享配置只留可提交的相对路径。
    #[test]
    fn machine_absolute_paths_are_split_out_of_shared_config() {
        let mut cfg = super::AppConfig::default();
        // 三个必填项各来一个绝对路径 + 一个相对路径
        cfg.paths.ffmpeg = r"A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe".to_string();
        cfg.paths.llama_cli = r"A:\cppsoft\llama.cpp\build\bin\Release\llama-completion.exe".to_string();
        cfg.paths.whisper_cli = "tools/whisper-vulkan/whisper.exe".to_string();
        // Option 项同样要有覆盖
        cfg.paths.punc_model = Some(r"D:\models\punc\model.int8.onnx".to_string());
        cfg.paths.sensevoice_model = Some("models/sensevoice/model.int8.onnx".to_string());

        let (shared, local) = cfg.split_machine_paths();

        // 绝对路径被挪走，共享配置回到便携默认值
        assert_eq!(shared.ffmpeg, super::PathsConfig::default().ffmpeg);
        assert_eq!(shared.llama_cli, super::PathsConfig::default().llama_cli);
        assert_eq!(shared.punc_model, super::PathsConfig::default().punc_model);
        // 本来就是相对路径的不动
        assert_eq!(shared.whisper_cli, "tools/whisper-vulkan/whisper.exe");
        assert_eq!(
            shared.sensevoice_model.as_deref(),
            Some("models/sensevoice/model.int8.onnx")
        );

        // 机器路径原样记录在本地覆盖里
        assert_eq!(local.ffmpeg.as_deref(), Some(r"A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe"));
        assert_eq!(
            local.punc_model.as_deref(),
            Some(r"D:\models\punc\model.int8.onnx")
        );
        assert!(local.whisper_cli.is_none(), "相对路径不应进入本地覆盖");
        assert!(local.has_any());
    }

    /// 全便携的配置不应产生本地覆盖文件。
    #[test]
    fn portable_config_produces_no_local_override() {
        let cfg = super::AppConfig::default();
        let (shared, local) = cfg.split_machine_paths();
        assert!(!local.has_any(), "默认配置全是相对路径，不该写本地覆盖");
        assert_eq!(shared.ffmpeg, cfg.paths.ffmpeg);
        assert_eq!(shared.llama_cli, cfg.paths.llama_cli);
    }

    /// 默认配置必须是便携的——否则「没有 config.toml 时自动生成的默认文件」
    /// 会把开发机路径写进去，再被提交给别人。
    #[test]
    fn default_config_paths_are_portable() {
        let cfg = super::AppConfig::default();
        for (name, value) in [
            ("ffmpeg", Some(cfg.paths.ffmpeg.clone())),
            ("whisper_cli", Some(cfg.paths.whisper_cli.clone())),
            ("whisper_model", Some(cfg.paths.whisper_model.clone())),
            ("llama_cli", Some(cfg.paths.llama_cli.clone())),
            ("llm_model", Some(cfg.paths.llm_model.clone())),
            ("vad_model", cfg.paths.vad_model.clone()),
            ("punc_model", cfg.paths.punc_model.clone()),
            ("sensevoice_model", cfg.paths.sensevoice_model.clone()),
            ("sensevoice_tokens", cfg.paths.sensevoice_tokens.clone()),
            ("sensevoice_vad", cfg.paths.sensevoice_vad.clone()),
        ] {
            let v = value.expect(name);
            assert!(
                !std::path::Path::new(&v).is_absolute(),
                "默认 {name} 不应是绝对路径: {v}"
            );
        }
    }

    /// 保存→读取的往返：机器路径经 `config.local.toml` 回来后必须**一字不差**，
    /// 否则「UI 里改个主题」就会把用户配好的 ffmpeg 路径悄悄改成默认值。
    #[test]
    fn save_load_roundtrip_preserves_machine_paths() {
        let dir = std::env::temp_dir().join(format!("v2w_cfg_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let main_path = dir.join("config.toml");

        let mut cfg = super::AppConfig::default();
        cfg.paths.ffmpeg = r"A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe".to_string();
        cfg.paths.llama_cli = r"A:\cppsoft\llama.cpp\llama-completion.exe".to_string();
        cfg.pipeline.whisper_threads = 12;
        cfg.ui.theme = "light".to_string();

        // 直接写绝对路径（绕过 resolve_path 的项目根拼接）
        cfg.save_to_file(&main_path).expect("save");

        // 共享配置里不能出现机器路径
        let shared_text = std::fs::read_to_string(&main_path).expect("read shared");
        assert!(
            !shared_text.contains(r"A:\cppsoft"),
            "机器路径不应写进 config.toml:\n{shared_text}"
        );
        // 覆盖文件里必须有
        let local_path = dir.join(super::AppConfig::LOCAL_OVERRIDE);
        let local_text = std::fs::read_to_string(&local_path).expect("read local");
        assert!(local_text.contains(r"A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe"));

        // 读回来：机器路径复原，共享调参也保留
        let back = super::AppConfig::load_from_file(&main_path).expect("load");
        assert_eq!(back.paths.ffmpeg, r"A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe");
        assert_eq!(back.paths.llama_cli, r"A:\cppsoft\llama.cpp\llama-completion.exe");
        assert_eq!(back.pipeline.whisper_threads, 12);
        assert_eq!(back.ui.theme, "light");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 内容没变时 `save_to_file` 不应重写文件（避免 UI 每敲一个字符都做一次
    /// 写盘 + rename）。用修改时间做证据：第二次保存后 mtime 必须不变。
    #[test]
    fn save_skips_disk_write_when_content_unchanged() {
        let dir = std::env::temp_dir().join(format!("v2w_cfg_idem_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let main_path = dir.join("config.toml");

        let cfg = super::AppConfig::default();
        cfg.save_to_file(&main_path).expect("first save");
        let mtime1 = std::fs::metadata(&main_path).expect("meta").modified().expect("mtime");

        // 确保时间戳可区分（一些文件系统粒度较粗）
        std::thread::sleep(std::time::Duration::from_millis(20));
        cfg.save_to_file(&main_path).expect("second save");
        let mtime2 = std::fs::metadata(&main_path).expect("meta").modified().expect("mtime");

        assert_eq!(mtime1, mtime2, "内容未变时不应重写文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 覆盖文件损坏时不能把启动带崩——退回 config.toml 即可。
    #[test]
    fn corrupt_local_override_falls_back_to_shared_config() {
        let dir = std::env::temp_dir().join(format!("v2w_cfg_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let main_path = dir.join("config.toml");

        let mut cfg = super::AppConfig::default();
        cfg.pipeline.whisper_threads = 7;
        cfg.save_to_file(&main_path).expect("save");

        // 写一个语法错误的覆盖文件
        std::fs::write(
            dir.join(super::AppConfig::LOCAL_OVERRIDE),
            "this is not valid toml = = =",
        )
        .expect("write bad override");

        let back = super::AppConfig::load_from_file(&main_path).expect("仍应能加载");
        assert_eq!(back.pipeline.whisper_threads, 7);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未知字段要报错而不是静默忽略：写错键名时用户能立刻发现。
    #[test]
    fn unknown_key_in_local_override_is_rejected() {
        let parsed: Result<super::LocalOverride, _> = toml::from_str(
            r#"
            [paths]
            ffmpegd = 'typo'
            "#,
        );
        assert!(parsed.is_err(), "拼错的键名应被拒绝");
    }
}