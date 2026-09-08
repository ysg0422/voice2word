//! 配置管理

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub paths: PathsConfig,
    pub pipeline: PipelineConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsConfig {
    pub ffmpeg: String,
    pub whisper_cli: String,
    pub whisper_model: String,
    #[serde(default)]
    pub vad_model: Option<String>,
    pub llama_cli: String,
    pub llm_model: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    pub language: String,
    pub output_format: String,
    pub enable_polish: bool,
    #[serde(default = "default_true")]
    pub enable_vad: bool,
    pub whisper_threads: u32,
    /// 0 表示按 CPU 核数自动推导；GPU 后端应设置为 1，避免争用单个设备。
    #[serde(default)]
    pub whisper_processors: u32,
    pub llm_threads: u32,
    pub llm_ctx: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            paths: PathsConfig {
                ffmpeg: "A:\\cppsoft\\ffmpeg-6.9\\bin\\ffmpeg.exe".to_string(),
                whisper_cli: "tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe".to_string(),
                whisper_model: "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string(),
                vad_model: Some("models/whisper/ggml-silero-v6.2.0.bin".to_string()),
                llama_cli: "A:\\cppsoft\\llama.cpp\\build\\bin\\Release\\llama-completion.exe".to_string(),
                llm_model: "models/llm/qwen2.5-0.5b-instruct-q4_k_m.gguf".to_string(),
            },
            pipeline: PipelineConfig {
                language: "zh".to_string(),
                output_format: "srt".to_string(),
                enable_polish: true,
                enable_vad: true,
                whisper_threads: 8,
                whisper_processors: 1,
                llm_threads: 8,
                llm_ctx: 4096,
            },
        }
    }
}

impl AppConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let full_path = Self::resolve_path(path.as_ref().to_str().unwrap_or("config.toml"));
        if full_path.exists() {
            let content = std::fs::read_to_string(&full_path)
                .with_context(|| format!("读取配置文件失败: {:?}", full_path))?;
            let cfg: AppConfig = toml::from_str(&content)
                .with_context(|| "反序列化 config.toml 失败")?;
            Ok(cfg)
        } else {
            let default_cfg = Self::default();
            default_cfg.save_to_file(&full_path)?;
            Ok(default_cfg)
        }
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let full_path = Self::resolve_path(path.as_ref().to_str().unwrap_or("config.toml"));
        let content = toml::to_string_pretty(self)
            .with_context(|| "序列化配置为 TOML 失败")?;
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full_path, content)?;
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
                if dir_name.eq_ignore_ascii_case("debug") || dir_name.eq_ignore_ascii_case("release") {
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
}
