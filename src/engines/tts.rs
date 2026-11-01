//! Edge-TTS 语音合成与时间线配音引擎
//! 支持基于微软 Edge-TTS 免费云端接口生成高质量配音，并按照字幕时间轴进行对齐混音与音轨替换。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

use crate::subtitle::Segment;
use crate::utils::AppConfig;

/// 预设的高质量 Edge-TTS 声音选项：(声音 ID, 展示名称, 语言, 性别)
pub const PRESET_VOICES: &[(&str, &str, &str, &str)] = &[
    ("zh-CN-XiaoxiaoNeural", "晓晓 (温柔自然)", "zh", "女"),
    ("zh-CN-YunxiNeural", "云希 (活泼生动)", "zh", "男"),
    ("zh-CN-YunjianNeural", "云健 (沉稳解说)", "zh", "男"),
    ("zh-CN-XiaoyiNeural", "晓伊 (亲切随和)", "zh", "女"),
    ("en-US-JennyNeural", "Jenny (英语标准)", "en", "女"),
    ("en-US-GuyNeural", "Guy (英语标准)", "en", "男"),
    ("ja-JP-NanamiNeural", "七海 (日语女声)", "ja", "女"),
    ("ja-JP-KeitaNeural", "圭太 (日语男声)", "ja", "男"),
];

/// 批量合成配置项
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct BatchItem {
    text: String,
    voice: String,
    output: String,
}

/// TTS 配音引擎
#[derive(Debug, Clone)]
pub struct TTSEngine {
    pub python_path: PathBuf,
    pub runner_path: PathBuf,
    /// 取消位（`Arc` 共享：`Clone` 出去的句柄取消同一个任务）。
    /// 与 whisper / sensevoice 引擎同一套语义：置位后在有界等待的轮询里被看到，
    /// 杀掉子进程并以「已取消」收尾。
    cancel: Arc<AtomicBool>,
}

impl TTSEngine {
    /// 构造 TTS 引擎实例，自动解析 Python 解释器与 runner 脚本
    ///
    /// 解释器必须走 [`AppConfig::resolve_command`]（内置便携 Python 优先 → PATH 兜底），
    /// 不能用 `resolve_path("tools/python.exe")`：
    /// - 内置运行时的真实路径是 `tools/python/python.exe`（`tools/python.exe` **不存在**），
    ///   写死那个路径 → TTS 永远 spawn 失败；
    /// - `resolve_command` 还会在内置缺失时回退到 PATH 上的系统 Python，
    ///   与其它引擎（SenseVoice / CT-Punc）保持同一套解析口径。
    pub fn new() -> Self {
        let python_path = AppConfig::resolve_command("python");
        let runner_path = AppConfig::resolve_path("tools/edge_tts_runner.py");
        Self {
            python_path,
            runner_path,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 检查 TTS 执行脚本是否存在
    pub fn is_available(&self) -> bool {
        self.runner_path.is_file()
    }

    /// 请求取消当前合成（可重复调用）。下一单要能正常跑时用 [`Self::begin_task`] 复位。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// 新任务入口清掉残留取消位（与 `TaskPipeline::begin_task` 同一语义：
    /// 复位点唯一，避免上一单的取消把下一单秒杀）。
    pub fn begin_task(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }

    /// 当前取消位是否已置位（编排层的取消桥接与测试用）。
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// 合成单句文本至目标音频文件 (MP3 或 WAV)
    pub fn synthesize_sentence<P: AsRef<Path>>(
        &self,
        text: &str,
        voice: &str,
        output_path: P,
    ) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let out = output_path.as_ref();
        info!("Edge-TTS 合成文本: {:?}, 音色: {}", text, voice);

        let args: Vec<OsString> = vec![
            "--text".into(),
            text.into(),
            "--voice".into(),
            voice.into(),
            "--output".into(),
            out.as_os_str().to_os_string(),
        ];
        let (status, stdout, stderr) = self.run_runner(&args, tts_wait_budget(text), None)?;
        if !status.success() {
            anyhow::bail!(
                "Edge-TTS 合成失败: {}{}",
                String::from_utf8_lossy(&stderr),
                String::from_utf8_lossy(&stdout)
            );
        }
        Ok(())
    }

    /// 批量逐句合成（配音闭环用）：一次 python 进程跑完 N 句。
    ///
    /// # 为什么走 runner 的 `--batch-json`
    ///
    /// 逐句 spawn 会让每句都付一次 Python 启动 + `edge_tts` 导入（约 0.3~0.8s），
    /// 一百句就是几十秒纯开销；runner 的批量模式在同一个事件循环里顺序合成，
    /// 还能顺带回报 `[PROGRESS] i/n`。失败句子不影响其余句子（runner 逐项执行），
    /// 但整体退出码非零时由调用方按「逐项检查产物是否存在」处理。
    ///
    /// `on_progress` 在**读 stdout 的线程里**被调用（i 从 1 起，n 为总数），
    /// 供 UI 显示「合成 37/214」；回调必须轻量，不要在里面碰 UI。
    pub fn synthesize_batch(
        &self,
        items: &[DubItem],
        on_progress: Option<BatchProgress>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        // runner 只认 {text, voice, rate, pitch, volume, output}；这里只暴露用到的四项。
        #[derive(Serialize)]
        struct Entry<'a> {
            text: &'a str,
            voice: &'a str,
            rate: String,
            output: String,
        }
        let payload: Vec<Entry<'_>> = items
            .iter()
            .map(|item| Entry {
                text: item.text.as_str(),
                voice: item.voice.as_str(),
                rate: format_rate(item.rate_percent),
                output: item.output.to_string_lossy().into_owned(),
            })
            .collect();
        let json = serde_json::to_string(&payload).context("序列化配音批量任务失败")?;

        // 任务单落临时文件：命令行长度有限（几百句的 JSON 会超），且 runner 只收路径。
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let job_path =
            std::env::temp_dir().join(format!("v2w_tts_batch_{}_{ms}.json", std::process::id()));
        let guard = crate::utils::TempPathGuard::file(job_path.clone());
        std::fs::write(guard.path(), json.as_bytes())
            .with_context(|| format!("写入配音任务单失败: {}", guard.path().display()))?;

        let args: Vec<OsString> = vec![
            "--batch-json".into(),
            guard.path().as_os_str().to_os_string(),
        ];
        let budget = tts_batch_budget(items.len());
        let total = items.len();
        let (status, stdout, stderr) = self.run_runner(
            &args,
            budget,
            on_progress.map(|mut cb| {
                Box::new(move |line: &str| {
                    // runner 逐项打印 `[PROGRESS] i/n`；解析失败的行直接忽略。
                    if let Some(rest) = line.strip_prefix("[PROGRESS] ") {
                        let mut parts = rest.trim().split('/');
                        if let (Some(i), Some(n)) = (parts.next(), parts.next()) {
                            if let (Ok(i), Ok(n)) = (i.trim().parse(), n.trim().parse()) {
                                cb(i, n);
                            }
                        }
                    }
                }) as Box<dyn FnMut(&str) + Send>
            }),
        )?;
        if !status.success() {
            // 失败也可能有部分句子成功，交由调用方按产物存在性判断。
            anyhow::bail!(
                "Edge-TTS 批量合成返回非零退出码（{total} 句）：{}{}",
                String::from_utf8_lossy(&stderr),
                String::from_utf8_lossy(&stdout)
            );
        }
        Ok(())
    }

    /// 起 runner、双端抽干、有界等待（超时/取消即 kill）——单句与批量共用这一份。
    ///
    /// 返回 `(退出状态, stdout 尾段, stderr)`；stdout 只保留最后 64KB（批量模式
    /// 每句一行进度，全留没有意义），stderr 全量保留（错误信息通常很短）。
    fn run_runner(
        &self,
        args: &[OsString],
        budget: Duration,
        on_stdout_line: Option<LineCallback>,
    ) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
        let mut cmd = Command::new(&self.python_path);
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        // Python 环境清洗（剥 PYTHONHOME/PYTHONPATH、强制 UTF-8）与 punc /
        // sensevoice 同一口径：用户机器上残留的 PYTHONHOME 会让任何 Python 子进程
        // 报「No module named encodings」，此前这条旁路漏掉了清洗。
        super::media_pipeline::apply_python_env(&mut cmd);
        cmd.arg(&self.runner_path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("执行 Edge-TTS 脚本失败: {:?}", self.runner_path))?;

        // stdout/stderr 各自抽干再等退出：管道缓冲只有 64KB，子进程写满会与父进程
        // 互等（与 punc / sensevoice 的双端抽干同一口径）。
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let stdout_reader = std::thread::spawn(move || {
            let mut kept: Vec<u8> = Vec::new();
            let mut cb = on_stdout_line;
            if let Some(pipe) = stdout_pipe.take() {
                for line in BufReader::new(pipe).lines() {
                    let Ok(line) = line else { continue };
                    if let Some(f) = cb.as_mut() {
                        f(&line);
                    }
                    kept.extend_from_slice(line.as_bytes());
                    kept.push(b'\n');
                    if kept.len() > 64 * 1024 {
                        let cut = kept.len() - 64 * 1024;
                        kept.drain(..cut);
                    }
                }
            }
            kept
        });
        let stderr_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stderr_pipe.take() {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });

        // 有界等待：Edge-TTS 是**联网服务**，网络挂起时 `cmd.output()` 会永久卡死
        // 且无法取消。
        let deadline = Instant::now() + budget;
        let status = loop {
            if self.cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                anyhow::bail!("Edge-TTS 合成已取消");
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_reader.join();
                        let _ = stderr_reader.join();
                        anyhow::bail!(
                            "Edge-TTS 合成超时（预算 {}s）：网络服务无响应，已终止子进程",
                            budget.as_secs()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(err).context("等待 Edge-TTS 脚本退出失败");
                }
            }
        };

        let stdout = stdout_reader.join().unwrap_or_default();
        let stderr = stderr_reader.join().unwrap_or_default();
        Ok((status, stdout, stderr))
    }

    /// 批量为字幕片段生成配音任务清单（纯函数，便于测试）
    ///
    /// 文件名带进程号：两个导出任务并发跑（或崩溃残留的旧文件）时，
    /// `tts_seg_{index}.mp3` 会互相覆盖——与 ffmpeg 的 `temp_wav_path`
    /// （pid + 原子序号）同一类防护；同一批内靠字幕 index 区分。
    pub fn plan_batch_items(
        segments: &[Segment],
        voice: &str,
        temp_dir: &Path,
    ) -> Vec<(usize, String, String, PathBuf)> {
        let pid = std::process::id();
        segments
            .iter()
            .filter(|s| !s.display_text().trim().is_empty())
            .map(|s| {
                let out_file = temp_dir.join(format!("tts_seg_{}_{}.mp3", pid, s.index));
                (
                    s.index,
                    s.display_text().to_string(),
                    voice.to_string(),
                    out_file,
                )
            })
            .collect()
    }
}

/// 单句 Edge-TTS 的等待预算：基础 60s + 每 20 字 1s，封顶 300s。
fn tts_wait_budget(text: &str) -> Duration {
    let extra = (text.chars().count() as u64 / 20).min(240);
    Duration::from_secs(60 + extra)
}

/// runner 的 stdout 逐行回调（批量模式解析 `[PROGRESS] i/n` 用）。
///
/// 抽成别名有两个理由：`clippy::type_complexity` 不接受内联的
/// `Box<dyn FnMut(&str) + Send>`；调用处写起来也短。
pub type LineCallback = Box<dyn FnMut(&str) + Send>;

/// 批量合成进度回调：`(已完成, 总数)`，i 从 1 起。
pub type BatchProgress = Box<dyn FnMut(usize, usize) + Send>;

/// 一句配音任务（[`TTSEngine::synthesize_batch`] 的输入）。
#[derive(Debug, Clone)]
pub struct DubItem {
    /// 对应 `Segment::index`。
    pub index: usize,
    pub text: String,
    /// Edge-TTS 音色 ID（见 [`PRESET_VOICES`] 或用户自定义）。
    pub voice: String,
    /// Edge-TTS `--rate` 百分数：0 = 原速，正数 = 更快（配音时长超出时间窗时用）。
    pub rate_percent: i32,
    pub output: PathBuf,
}

/// 百分数 → Edge-TTS 的 `+N%` / `-N%` 字符串（0 也写成 `+0%`，与 runner 默认一致）。
pub fn format_rate(percent: i32) -> String {
    if percent >= 0 {
        format!("+{percent}%")
    } else {
        format!("{percent}%")
    }
}

/// 批量合成的等待预算：60s 基础 + 每句 6s，封顶 2 小时。
///
/// 单句合成实测 0.5~1.5s（含 TLS 复用），6s/句 已足够宽裕；封顶是为了让「一千句
/// 的整片配音」在异常时也能在 2 小时内失败退出，而不是无限等。
pub fn tts_batch_budget(items: usize) -> Duration {
    let secs = 60u64
        .saturating_add((items as u64).saturating_mul(6))
        .min(7200);
    Duration::from_secs(secs.max(60))
}

/// 按说话人挑音色：语言匹配的预设里按 `speaker % len` 轮换。
///
/// - `lang` 取目标语言或句子的 `language`，取前两位做前缀（`zh` / `en` / `ja`）；
/// - 该语言没有预设时回退中文组，再回退到全部预设；
/// - `override_voice` 非空时直接用它（配置里指定的首选音色，优先级最高）。
///
/// 为什么按 speaker 轮换而不是全片一个声音：多人对话里「同一个人同一个声音」是
/// 听众区分说话人的第一线索，Edge-TTS 免费提供多音色，不用白不用。真正的
/// 声纹克隆（F5-TTS/CosyVoice）留作后续可选后端。
pub fn voice_for_speaker(lang: &str, speaker: Option<u32>, override_voice: Option<&str>) -> String {
    if let Some(v) = override_voice.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    let lang = lang.trim().to_ascii_lowercase();
    let prefix: String = lang.chars().take(2).collect();
    let mut group: Vec<&str> = PRESET_VOICES
        .iter()
        .filter(|(_, _, l, _)| *l == prefix)
        .map(|(id, _, _, _)| *id)
        .collect();
    if group.is_empty() {
        group = PRESET_VOICES
            .iter()
            .filter(|(_, _, l, _)| *l == "zh")
            .map(|(id, _, _, _)| *id)
            .collect();
    }
    if group.is_empty() {
        group = PRESET_VOICES.iter().map(|(id, _, _, _)| *id).collect();
    }
    if group.is_empty() {
        return "zh-CN-XiaoxiaoNeural".to_string();
    }
    let idx = speaker.unwrap_or(0) as usize % group.len();
    group[idx].to_string()
}

impl Default for TTSEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_voices_are_valid() {
        assert!(!PRESET_VOICES.is_empty());
        for (id, name, lang, gender) in PRESET_VOICES {
            assert!(!id.is_empty());
            assert!(!name.is_empty());
            assert!(!lang.is_empty());
            assert!(!gender.is_empty());
        }
    }

    #[test]
    fn plan_batch_items_filters_empty_text() {
        let segs = vec![
            Segment::new(1, 0.0, 1.0, "第一句"),
            Segment::new(2, 1.0, 2.0, "   "),
            Segment::new(3, 2.0, 3.0, "第三句"),
        ];
        let items = TTSEngine::plan_batch_items(&segs, "zh-CN-XiaoxiaoNeural", Path::new("temp"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, 1);
        assert_eq!(items[0].1, "第一句");
        assert_eq!(items[0].2, "zh-CN-XiaoxiaoNeural");
        assert_eq!(items[1].0, 3);
        assert_eq!(items[1].1, "第三句");
        assert_eq!(items[1].2, "zh-CN-XiaoxiaoNeural");
    }

    /// 回归：临时文件名必须带进程号。修前 `tts_seg_{index}.mp3` 只按字幕序号命名，
    /// 两个并发导出任务（或崩溃残留的旧文件）会互相覆盖。
    #[test]
    fn plan_batch_items_names_are_process_scoped() {
        let segs = vec![Segment::new(7, 0.0, 1.0, "一句")];
        let items = TTSEngine::plan_batch_items(&segs, "v", Path::new("temp"));
        let name = items[0]
            .3
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.contains(&std::process::id().to_string()),
            "文件名必须带进程号防并发覆盖: {name}"
        );
    }

    /// 等待预算随句长放宽且有封顶：Edge-TTS 是联网服务，无界等待等于网络一挂就永久卡死。
    #[test]
    fn wait_budget_scales_with_text_and_caps() {
        assert_eq!(tts_wait_budget(""), Duration::from_secs(60));
        assert_eq!(tts_wait_budget(&"字".repeat(40)), Duration::from_secs(62));
        assert_eq!(
            tts_wait_budget(&"字".repeat(100_000)),
            Duration::from_secs(300),
            "超长文本必须封顶，不能无界"
        );
    }

    /// 取消位语义与其它引擎一致：置位生效、`begin_task` 复位。
    #[test]
    fn cancel_flag_roundtrips_with_begin_task() {
        let engine = TTSEngine::new();
        engine.cancel();
        assert!(engine.cancel.load(Ordering::SeqCst));
        engine.begin_task();
        assert!(!engine.cancel.load(Ordering::SeqCst));
    }

    /// `--rate` 的字符串格式：正数带 `+`，负数带 `-`，0 写 `+0%`。
    #[test]
    fn rate_is_formatted_for_edge_tts() {
        assert_eq!(format_rate(0), "+0%");
        assert_eq!(format_rate(12), "+12%");
        assert_eq!(format_rate(-5), "-5%");
    }

    /// 批量预算随句数线性增长并封顶（异常时不无限等）。
    #[test]
    fn batch_budget_scales_and_caps() {
        assert_eq!(tts_batch_budget(0), Duration::from_secs(60));
        assert_eq!(tts_batch_budget(10), Duration::from_secs(120));
        assert_eq!(tts_batch_budget(100_000), Duration::from_secs(7200));
    }

    /// 说话人 → 音色：中文组轮换、英文组走英文、override 优先、未知语言回退。
    #[test]
    fn voice_rotation_follows_speaker_and_language() {
        let zh0 = voice_for_speaker("zh", Some(0), None);
        let zh1 = voice_for_speaker("zh", Some(1), None);
        assert!(zh0.starts_with("zh-CN-"), "zh 应选中文音色: {zh0}");
        assert_ne!(zh0, zh1, "不同说话人应轮换到不同音色");

        let en = voice_for_speaker("en", Some(0), None);
        assert!(en.starts_with("en-US-"), "en 应选英文音色: {en}");

        assert_eq!(
            voice_for_speaker("zh", Some(3), Some("custom-voice")),
            "custom-voice",
            "配置里指定的音色优先"
        );
        // 未知语言回退中文组（不 panic、不返回空）
        let fallback = voice_for_speaker("xx", None, None);
        assert!(!fallback.is_empty());
    }
}
