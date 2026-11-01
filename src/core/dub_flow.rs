//! 配音闭环编排：TTS 逐句合成 → 时长测量 → 语速自适应 → 时间轴混音 → 替换/混音出片。
//!
//! 设计见 `docs/配音闭环-设计与实现.md`。本模块只做「流程」，纯计算部分在
//! [`crate::subtitle::dub`]（时间轴规划）与 [`crate::engines::tts`]（合成/音色）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::engines::tts::{DubItem, TTSEngine};
use crate::engines::FFmpegEngine;
use crate::subtitle::dub::{mix_track, plan_placements, required_rate_percent};
use crate::subtitle::Segment;
use crate::utils::config::DubConfig;

/// 配音采样率：Edge-TTS 原生输出 24kHz MP3，按 24k 解码免一次重采样。
pub const DUB_SAMPLE_RATE: u32 = 24_000;

/// 配音文本来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DubSource {
    /// 译文优先（为空时回退原文）。
    Translation,
    /// 原文。
    Text,
}

/// 一次配音的运行参数（由配置 + 界面临时选择合成）。
#[derive(Debug, Clone, PartialEq)]
pub struct DubOptions {
    pub source: DubSource,
    /// 0 = 纯替换音轨；>0 = 保留原声并按该比例压低后混音。
    pub bgm_volume: f32,
    /// 语速上限（百分数）。
    pub max_rate_percent: u32,
    pub voice_zh: String,
    pub voice_en: String,
}

impl DubOptions {
    /// 从 `[dub]` 配置解析。`mode = replace` 等价于 `bgm_volume = 0`。
    pub fn from_config(cfg: &DubConfig) -> Self {
        let replace = cfg.mode.trim().eq_ignore_ascii_case("replace");
        let bgm = if replace {
            0.0
        } else {
            cfg.bgm_volume.clamp(0.0, 1.0)
        };
        Self {
            source: parse_source(&cfg.source),
            bgm_volume: bgm,
            max_rate_percent: cfg.max_rate_percent.clamp(1, 80),
            voice_zh: cfg.voice_zh.clone(),
            voice_en: cfg.voice_en.clone(),
        }
    }
}

/// 解析 `source`：未知值回退「译文优先」（与配置注释一致）。
pub fn parse_source(raw: &str) -> DubSource {
    if raw.trim().eq_ignore_ascii_case("text") {
        DubSource::Text
    } else {
        DubSource::Translation
    }
}

/// 一句要读的文本：按来源选，空译文回退原文，全空返回 `None`（该句不配音）。
pub fn dub_text_for(seg: &Segment, source: DubSource) -> Option<String> {
    let translated = match source {
        DubSource::Translation => seg
            .translation
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty()),
        DubSource::Text => None,
    };
    let text = translated
        .map(str::to_string)
        .unwrap_or_else(|| seg.display_text().trim().to_string());
    (!text.is_empty()).then_some(text)
}

/// 配音结果摘要（界面提示与 CLI 输出用）。
#[derive(Debug, Clone, Default)]
pub struct DubReport {
    pub synthesized: usize,
    /// 合成失败的句子 `Segment::index`。
    pub failed: Vec<usize>,
    /// 因超窗做了语速补偿的句数。
    pub sped_up: usize,
    /// 因压到下一句而被截断的句数。
    pub trimmed: usize,
    pub output: PathBuf,
    pub elapsed_sec: f64,
}

/// 配音音色：按句子语言 + 说话人选择（`voice_zh/en` 为空时按预设轮换）。
fn voice_for(seg: &Segment, target_lang: &str, opts: &DubOptions) -> String {
    // 读译文时按译文语言选声音，回退原文时才用源语言。
    let use_translation = opts.source == DubSource::Translation && seg.has_translation();
    let lang = if use_translation {
        seg.translation_lang.as_deref().unwrap_or(target_lang)
    } else {
        seg.language.as_deref().unwrap_or(target_lang)
    };
    let lang = lang.trim();
    let lang = if lang.is_empty() {
        target_lang.trim()
    } else {
        lang
    };
    let lang = lang.split(['-', '_']).next().unwrap_or(lang);
    let lang = match lang {
        "English" | "english" | "英语" | "英文" => "en",
        "日本語" | "日语" | "Japanese" | "japanese" => "ja",
        "简体中文" | "繁体中文" | "中文" | "Chinese" | "chinese" => "zh",
        other => other,
    };
    let override_voice = if lang.eq_ignore_ascii_case("en") {
        Some(opts.voice_en.as_str())
    } else if lang.eq_ignore_ascii_case("zh") {
        Some(opts.voice_zh.as_str())
    } else {
        None
    }
    .filter(|v| !v.trim().is_empty());
    crate::engines::tts::voice_for_speaker(lang, seg.speaker, override_voice)
}

/// 配音进度回调：`(阶段文案, 0.0~1.0)`。
///
/// 用 `Arc<dyn Fn + Send + Sync>` 而不是 `&dyn Fn`：合成进度是在 runner 的 stdout
/// 读线程里上报的，跨线程要求 `'static`；调用方用 channel / 共享快照自行落地。
pub type ProgressFn = Arc<dyn Fn(&str, f32) + Send + Sync>;

/// 跑完整条配音链路。
///
/// `progress` 收 `(阶段文案, 0.0~1.0)`；`cancel` 置位后尽快退出（已合成的临时文件
/// 由临时目录整体删除）。
///
/// 回调用 `Arc<dyn Fn + Send + Sync>` 而不是 `&dyn Fn`：合成进度是在 runner 的
/// stdout 读线程里上报的，跨线程要求 `'static`；调用方用 channel/原子量自行落地。
#[allow(clippy::too_many_arguments)]
pub fn run_dub(
    ffmpeg: &FFmpegEngine,
    tts: &TTSEngine,
    media: &Path,
    segments: &[Segment],
    total_secs: f64,
    target_lang: &str,
    output: &Path,
    opts: &DubOptions,
    progress: ProgressFn,
    cancel: &Arc<AtomicBool>,
) -> Result<DubReport> {
    let started = Instant::now();
    if segments.is_empty() {
        anyhow::bail!("没有字幕可配音");
    }
    if !tts.is_available() {
        anyhow::bail!("配音组件未就位：缺少 tools/edge_tts_runner.py");
    }

    let work_dir = std::env::temp_dir().join(format!(
        "v2w_dub_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&work_dir)
        .with_context(|| format!("创建配音临时目录失败: {}", work_dir.display()))?;

    // 桥接调用方取消位与 TTS 引擎自己的取消位：引擎内部本来就有取消位
    // （run_runner 会轮询并 kill 子进程），但编排层从未置位它——用户点取消后，
    // 批量合成要等整批跑完（联网时最长两小时预算）才会在阶段之间被看到。
    // 看门线程负责把「用户取消」转成「引擎取消」，让正在跑的 python 进程立刻被终止。
    tts.begin_task();
    let stop = Arc::new(AtomicBool::new(false));
    struct StopGuard(Arc<AtomicBool>);
    impl Drop for StopGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let stop_guard = StopGuard(Arc::clone(&stop));
    let watcher = spawn_cancel_bridge(tts.clone(), Arc::clone(cancel), Arc::clone(&stop));

    let result = run_dub_inner(
        ffmpeg,
        tts,
        media,
        segments,
        total_secs,
        target_lang,
        output,
        opts,
        progress,
        cancel,
        &work_dir,
    );
    // 先停看门线程再回收：否则它会一直轮询到进程退出（取消位在任务结束后没有意义）。
    drop(stop_guard);
    if let Ok(handle) = watcher {
        let _ = handle.join();
    }
    // 无论成败都清掉临时目录：成功时中间产物没用了，失败时更不能留一堆残片。
    if let Err(err) = std::fs::remove_dir_all(&work_dir) {
        warn!(error = %err, dir = %work_dir.display(), "清理配音临时目录失败（启动清扫会兜底）");
    }

    let mut report = result?;
    report.elapsed_sec = started.elapsed().as_secs_f64();
    Ok(report)
}

/// 看门线程：用户取消置位后立刻置位 TTS 引擎的取消位（正在跑的合成进程会被 kill）。
fn spawn_cancel_bridge(
    tts: TTSEngine,
    user_cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            if user_cancel.load(Ordering::SeqCst) {
                tts.cancel();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn run_dub_inner(
    ffmpeg: &FFmpegEngine,
    tts: &TTSEngine,
    media: &Path,
    segments: &[Segment],
    total_secs: f64,
    target_lang: &str,
    output: &Path,
    opts: &DubOptions,
    progress: ProgressFn,
    cancel: &Arc<AtomicBool>,
    work_dir: &Path,
) -> Result<DubReport> {
    // ① 组任务：文本为空/无效时间轴的句子跳过。
    struct Job {
        index: usize,
        start: f64,
        end: f64,
        text: String,
        voice: String,
        output: PathBuf,
    }
    let mut jobs: Vec<Job> = Vec::new();
    for seg in segments {
        if !seg.start.is_finite() || !seg.end.is_finite() || seg.end <= seg.start {
            continue;
        }
        let Some(text) = dub_text_for(seg, opts.source) else {
            continue;
        };
        jobs.push(Job {
            index: seg.index,
            start: seg.start,
            end: seg.end,
            text,
            voice: voice_for(seg, target_lang, opts),
            output: work_dir.join(format!("seg_{:05}.mp3", seg.index)),
        });
    }
    if jobs.is_empty() {
        anyhow::bail!("没有可配音的文本（译文与原文都为空）");
    }
    progress("准备配音任务", 0.02);

    // ② 第一遍：原速合成。
    let items: Vec<DubItem> = jobs
        .iter()
        .map(|j| DubItem {
            index: j.index,
            text: j.text.clone(),
            voice: j.voice.clone(),
            rate_percent: 0,
            output: j.output.clone(),
        })
        .collect();
    let total_items = items.len();
    info!(count = total_items, "开始配音合成（第一遍，原速）");
    let mut synth_note: Option<String> = None;
    {
        let progress = Arc::clone(&progress);
        let cb = move |i: usize, n: usize| {
            let frac = if n == 0 { 0.0 } else { i as f32 / n as f32 };
            progress("合成配音（原速）", 0.05 + frac * 0.55);
        };
        if let Err(err) = tts.synthesize_batch(&items, Some(Box::new(cb))) {
            // 逐项失败（某句合成异常）时 runner 会中断，但已完成句子的文件还在；
            // 只有「一个产物都没有」才当成致命错误。
            let produced = jobs.iter().filter(|j| j.output.is_file()).count();
            if produced == 0 {
                return Err(err).context("配音合成失败（没有任何句子合成成功）");
            }
            warn!(error = %err, produced, "配音合成部分失败，继续处理已完成的句子");
            synth_note = Some(format!("{err:#}"));
        }
    }
    if cancel.load(Ordering::SeqCst) {
        anyhow::bail!("配音已取消");
    }

    // ③ 解码测时长；失败的句子记下来按静音处理。
    progress("测量配音时长", 0.62);
    let mut durations: Vec<f64> = Vec::with_capacity(jobs.len());
    let mut pcms: Vec<Vec<i16>> = Vec::with_capacity(jobs.len());
    let mut failed: Vec<usize> = Vec::new();
    for job in jobs.iter() {
        if cancel.load(Ordering::SeqCst) {
            anyhow::bail!("配音已取消");
        }
        match ffmpeg.decode_pcm_mono(&job.output, DUB_SAMPLE_RATE) {
            Ok(pcm) if !pcm.is_empty() => {
                durations.push(pcm.len() as f64 / DUB_SAMPLE_RATE as f64);
                pcms.push(pcm);
            }
            _ => {
                failed.push(job.index);
                durations.push(0.0);
                pcms.push(Vec::new());
            }
        }
    }

    // ④ 第二遍：只对「超窗」的句子提速重合成（见设计文档 4.2）。
    progress("按时间窗调整语速", 0.68);
    let mut sped_up_jobs: Vec<usize> = Vec::new();
    let mut retry_items: Vec<DubItem> = Vec::new();
    for (pos, job) in jobs.iter().enumerate() {
        let window = (job.end - job.start).max(0.0);
        let Some(percent) = required_rate_percent(durations[pos], window, opts.max_rate_percent)
        else {
            continue;
        };
        if pcms[pos].is_empty() {
            continue; // 上一轮就失败了，不必再试
        }
        sped_up_jobs.push(pos);
        retry_items.push(DubItem {
            index: job.index,
            text: job.text.clone(),
            voice: job.voice.clone(),
            rate_percent: percent as i32,
            output: job.output.clone(),
        });
    }
    if !retry_items.is_empty() {
        let retry_total = retry_items.len();
        info!(count = retry_total, "第二遍合成（提速以贴合时间窗）");
        {
            let progress = Arc::clone(&progress);
            let cb = move |i: usize, n: usize| {
                let frac = if n == 0 { 0.0 } else { i as f32 / n as f32 };
                progress("合成配音（提速）", 0.68 + frac * 0.10);
            };
            if let Err(err) = tts.synthesize_batch(&retry_items, Some(Box::new(cb))) {
                // 提速合成失败不算致命：保留原速版本，靠放置规则的溢出/截断兜底。
                warn!(error = %err, "提速合成失败，保留原速版本");
            }
        }
        // 重新解码提速后的音频；解不出来就退回原速那版。
        for &pos in sped_up_jobs.iter() {
            let job = &jobs[pos];
            match ffmpeg.decode_pcm_mono(&job.output, DUB_SAMPLE_RATE) {
                Ok(pcm) if !pcm.is_empty() => {
                    durations[pos] = pcm.len() as f64 / DUB_SAMPLE_RATE as f64;
                    pcms[pos] = pcm;
                }
                _ => warn!(index = job.index, "提速后的音频解码失败，保留原速版本"),
            }
        }
    }
    if cancel.load(Ordering::SeqCst) {
        anyhow::bail!("配音已取消");
    }

    // ⑤ 时间轴放置 + 混音（跳过失败的句子）。
    progress("对齐时间轴并混音", 0.80);
    let mut entries: Vec<(usize, f64, f64)> = Vec::with_capacity(jobs.len());
    let mut keep_pcms: Vec<Vec<i16>> = Vec::with_capacity(jobs.len());
    for (pos, job) in jobs.iter().enumerate() {
        if pcms[pos].is_empty() {
            continue;
        }
        entries.push((job.index, job.start, job.end));
        keep_pcms.push(std::mem::take(&mut pcms[pos]));
    }
    let durations_kept = durations_for(&keep_pcms);
    let placements = plan_placements(&entries, &durations_kept, DUB_SAMPLE_RATE, total_secs);
    let trimmed = placements.iter().filter(|p| p.trimmed).count();
    let total_samples = (if total_secs > 0.0 { total_secs } else { 0.0 } * DUB_SAMPLE_RATE as f64)
        .round()
        .max(1.0) as usize;
    // 总长取「规划结果实际覆盖到的终点」与目标时长的较大者，避免末句被切。
    let last_end = placements
        .iter()
        .map(|p| p.start_sample + p.len_samples)
        .max()
        .unwrap_or(0);
    let track_len = total_samples.max(last_end);
    let track = mix_track(&placements, &keep_pcms, track_len);

    let wav_path = work_dir.join("dub_track.wav");
    crate::engines::audio_prep::write_wav_mono16(&wav_path, &track, DUB_SAMPLE_RATE)
        .with_context(|| format!("写入配音音轨失败: {}", wav_path.display()))?;

    // ⑥ 替换 / 混音出片。
    progress("合成输出视频", 0.90);
    ffmpeg
        .replace_video_audio(media, &wav_path, output, opts.bgm_volume)
        .with_context(|| {
            format!(
                "配音合成失败（视频={}，音轨={}）",
                media.display(),
                wav_path.display()
            )
        })?;
    progress("配音完成", 1.0);

    if let Some(note) = &synth_note {
        warn!(detail = %note, "本轮配音存在合成失败的句子");
    }
    Ok(DubReport {
        synthesized: keep_pcms.len(),
        failed,
        sped_up: sped_up_jobs.len(),
        trimmed,
        output: output.to_path_buf(),
        elapsed_sec: 0.0,
    })
}

/// 与 `pcms` 同序的时长表（秒）。
fn durations_for(pcms: &[Vec<i16>]) -> Vec<f64> {
    pcms.iter()
        .map(|pcm| pcm.len() as f64 / DUB_SAMPLE_RATE as f64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(index: usize, start: f64, end: f64, text: &str) -> Segment {
        Segment::new(index, start, end, text)
    }

    /// 取消桥接：用户取消位一旦置位，引擎取消位必须在看门线程下一轮轮询内跟上。
    ///
    /// 这条钉的是「点了取消、界面却继续卡在合成配音」：引擎内部早就有取消位与
    /// kill 逻辑，缺的只是编排层把它接上。
    #[test]
    fn cancel_bridge_propagates_user_cancel_to_tts_engine() {
        let tts = TTSEngine::new();
        tts.begin_task();
        let user_cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let handle =
            spawn_cancel_bridge(tts.clone(), Arc::clone(&user_cancel), Arc::clone(&stop)).unwrap();
        assert!(!tts.is_cancelled());

        user_cancel.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !tts.is_cancelled() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(tts.is_cancelled(), "用户取消必须传导到 TTS 引擎");

        stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }

    /// 看门线程在 stop 置位后自行退出（不泄漏到进程结束）。
    #[test]
    fn cancel_bridge_exits_when_stopped() {
        let tts = TTSEngine::new();
        let user_cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let handle =
            spawn_cancel_bridge(tts.clone(), Arc::clone(&user_cancel), Arc::clone(&stop)).unwrap();
        stop.store(true, Ordering::SeqCst);
        handle.join().expect("看门线程应在 stop 后正常退出");
        assert!(!tts.is_cancelled());
    }

    #[test]
    fn source_parsing_falls_back_to_translation() {
        assert_eq!(parse_source("text"), DubSource::Text);
        assert_eq!(parse_source("TEXT"), DubSource::Text);
        assert_eq!(parse_source("translation"), DubSource::Translation);
        assert_eq!(parse_source("随便写"), DubSource::Translation);
    }

    #[test]
    fn options_map_replace_mode_to_zero_bgm() {
        let cfg = DubConfig {
            mode: "replace".to_string(),
            bgm_volume: 0.6,
            ..DubConfig::default()
        };
        let opts = DubOptions::from_config(&cfg);
        assert_eq!(opts.bgm_volume, 0.0, "replace 模式必须整条替换");

        let cfg = DubConfig {
            mode: "mix".to_string(),
            bgm_volume: 0.6,
            ..DubConfig::default()
        };
        let opts = DubOptions::from_config(&cfg);
        assert!((opts.bgm_volume - 0.6).abs() < 1e-6);
    }

    #[test]
    fn options_clamp_volume_and_rate() {
        let cfg = DubConfig {
            bgm_volume: 5.0,
            max_rate_percent: 999,
            ..DubConfig::default()
        };
        let opts = DubOptions::from_config(&cfg);
        assert_eq!(opts.bgm_volume, 1.0);
        assert_eq!(opts.max_rate_percent, 80);
    }

    #[test]
    fn dub_text_prefers_translation_and_falls_back() {
        let mut s = seg(1, 0.0, 1.0, "原文");
        s.translation = Some("译文".to_string());
        assert_eq!(
            dub_text_for(&s, DubSource::Translation).as_deref(),
            Some("译文")
        );
        assert_eq!(dub_text_for(&s, DubSource::Text).as_deref(), Some("原文"));

        // 译文为空 → 回退原文
        s.translation = Some("   ".to_string());
        assert_eq!(
            dub_text_for(&s, DubSource::Translation).as_deref(),
            Some("原文")
        );

        // 原文与译文都空 → 不配音
        let mut empty = seg(2, 0.0, 1.0, "   ");
        empty.polished = "  ".to_string();
        assert!(dub_text_for(&empty, DubSource::Translation).is_none());
    }

    #[test]
    fn voice_selection_respects_language_and_speaker() {
        let mut s = seg(1, 0.0, 1.0, "你好");
        s.language = Some("zh".to_string());
        let opts = DubOptions::from_config(&DubConfig::default());
        assert!(voice_for(&s, "zh", &opts).starts_with("zh-CN-"));

        s.language = Some("en".to_string());
        assert!(voice_for(&s, "zh", &opts).starts_with("en-US-"));

        // 配置里指定首选音色时优先
        let mut opts = opts;
        opts.voice_zh = "custom-zh".to_string();
        s.language = Some("zh".to_string());
        assert_eq!(voice_for(&s, "zh", &opts), "custom-zh");
    }

    #[test]
    fn dubbed_translation_uses_its_language_and_matching_override() {
        let mut s = seg(1, 0.0, 1.0, "你好");
        s.language = Some("zh".into());
        s.translation = Some("Hello".into());
        s.translation_lang = Some("English".into());
        let mut opts = DubOptions::from_config(&DubConfig::default());
        opts.voice_zh = "custom-zh".into();
        assert!(voice_for(&s, "English", &opts).starts_with("en-US-"));
        opts.voice_en = "custom-en".into();
        assert_eq!(voice_for(&s, "English", &opts), "custom-en");
        s.translation_lang = Some("日本語".into());
        assert!(voice_for(&s, "日本語", &opts).starts_with("ja-JP-"));
        opts.source = DubSource::Text;
        assert_eq!(voice_for(&s, "English", &opts), "custom-zh");
        opts.source = DubSource::Translation;
        s.translation = Some(" ".into());
        assert_eq!(voice_for(&s, "English", &opts), "custom-zh");
    }
}
