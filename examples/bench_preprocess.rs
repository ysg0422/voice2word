//! 端到端「音频预处理 + Whisper 转写」基准：把 src/core/pipeline.rs 的 Whisper 路径
//! 原样搬到一个无 UI 的可执行入口，用于量化预处理各阶段的耗时与最终 SRT 质量。
//!
//! 为什么需要它：CLI 基准只能测 whisper-cli 本身，无法回答「我们的语音增强 /
//! 停顿压实到底省了多少、精度代价多大」。这里复用生产代码（同一套滤镜链、
//! 同一套 CompactionPlan、同一个 WhisperEngine 参数），只把结果落成 SRT。
//!
//! 用法：
//!   cargo run --offline --release --example bench_preprocess -- <媒体> <起点秒> <时长秒> <输出srt> [--no-prep] [--no-compact]
//!
//! 例：
//!   cargo run --offline --release --example bench_preprocess -- testVideo/03.1.3概率不等式.mp4 300 600 target/prep_on.srt
//!   cargo run --offline --release --example bench_preprocess -- testVideo/03.1.3概率不等式.mp4 300 600 target/prep_off.srt --no-prep

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Instant;

use voice2word::engines::{
    plan_compaction, write_wav_mono16, CompactionConfig, CompactionPlan, FFmpegEngine,
    SpeechFilterOptions, WhisperEngine, ASR_SAMPLE_RATE,
};
use voice2word::subtitle::Segment;

fn format_time(sec: f64) -> String {
    let sec = sec.max(0.0);
    let h = (sec / 3600.0) as u32;
    let m = ((sec % 3600.0) / 60.0) as u32;
    let s = sec % 60.0;
    format!("{:02}:{:02}:{:06.3}", h, m, s).replace('.', ",")
}

fn write_srt(path: &Path, segments: &[Segment], offset: f64) -> Result<()> {
    let mut out = String::new();
    for (i, seg) in segments.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            format_time(seg.start + offset),
            format_time(seg.end + offset),
            seg.display_text()
        ));
    }
    std::fs::write(path, out).with_context(|| format!("写出 SRT 失败: {path:?}"))?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let media = PathBuf::from(args.first().context(
        "用法: bench_preprocess <媒体> <起点秒> <时长秒> <输出srt> [--no-prep] [--no-compact]",
    )?);
    let start: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let dur: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let out_srt = PathBuf::from(
        args.get(3)
            .cloned()
            .unwrap_or_else(|| "target/prep.srt".into()),
    );
    let no_prep = args.iter().any(|a| a == "--no-prep");
    let no_compact = args.iter().any(|a| a == "--no-compact");

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ffmpeg_path = if Path::new("A:\\cppsoft\\ffmpeg-6.9\\bin\\ffmpeg.exe").exists() {
        PathBuf::from("A:\\cppsoft\\ffmpeg-6.9\\bin\\ffmpeg.exe")
    } else {
        root.join("A:/cppsoft/ffmpeg-6.9/bin/ffmpeg.exe")
    };
    let ffmpeg = FFmpegEngine::new(&ffmpeg_path);

    let cli = root.join("tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe");
    let model = root.join("models/whisper/ggml-small-q5_0.bin");
    let vad = root.join("models/whisper/ggml-silero-v6.2.0.bin");

    let whisper = WhisperEngine::with_device(
        &cli,
        &model,
        Some(vad),
        12,
        1,
        false, // CPU：与既有基准口径一致，排除核显调度噪声
        true,  // no_fallback
        32,    // max_context
        100,
    );

    // 先把 start..start+dur 裁成独立 16kHz 单声道 WAV，避免整片时长影响后续策略
    let clip = std::env::temp_dir().join(format!("v2w_prepbench_{}.wav", std::process::id()));
    let t0 = Instant::now();
    ffmpeg.extract_audio_window(&media, start, dur, &clip)?;
    let extract_sec = t0.elapsed().as_secs_f64();
    println!("[裁片] {:.2}s -> {:?}", extract_sec, clip);

    let t_prep = Instant::now();
    let mut plan: Option<CompactionPlan> = None;
    let mut prep_note = String::from("未启用预处理");

    let asr_input: PathBuf = if no_prep {
        clip.clone()
    } else {
        let chain = SpeechFilterOptions::default().chain_with_speed(1.0);
        let pcm = ffmpeg.decode_pcm_mono_filtered(&clip, ASR_SAMPLE_RATE, chain.as_deref())?;
        let decoded_sec = pcm.len() as f64 / ASR_SAMPLE_RATE as f64;
        let p = if no_compact {
            CompactionPlan::identity(pcm.len(), ASR_SAMPLE_RATE)
        } else {
            plan_compaction(&pcm, ASR_SAMPLE_RATE, &CompactionConfig::default())
        };
        let use_plan = !p.is_identity() && p.is_worthwhile(0.10);
        let pcm_len = pcm.len();
        let kept: Vec<i16> = if use_plan { p.materialize(&pcm) } else { pcm };
        let wav =
            std::env::temp_dir().join(format!("v2w_prepbench_out_{}.wav", std::process::id()));
        write_wav_mono16(&wav, &kept, ASR_SAMPLE_RATE)?;
        prep_note = format!(
            "增强+压实 {:.1}s -> {:.1}s (切除 {:.0}%, 压实={})",
            decoded_sec,
            kept.len() as f64 / ASR_SAMPLE_RATE as f64,
            (1.0 - kept.len() as f64 / pcm_len.max(1) as f64) * 100.0,
            use_plan
        );
        if use_plan {
            plan = Some(p);
        }
        wav
    };
    let prep_sec = t_prep.elapsed().as_secs_f64();
    println!("[预处理] {:.2}s | {}", prep_sec, prep_note);

    let asr_sec_dur = ffmpeg.get_duration(&asr_input);
    let t_asr = Instant::now();
    let (mut segments, _vad) = whisper.transcribe_with_model(
        &asr_input,
        Some("zh"),
        Some(12),
        Some(asr_sec_dur),
        None,
        None,
    )?;
    let asr_sec = t_asr.elapsed().as_secs_f64();

    // 时间轴还原：先查压实映射表，再乘变速倍率（此处 speed=1.0）
    if let Some(p) = plan.as_ref() {
        for seg in segments.iter_mut() {
            let (s, e) = p.to_original_span(seg.start, seg.end);
            seg.start = s;
            seg.end = e;
        }
    }
    voice2word::subtitle::optimize_segments(&mut segments);

    write_srt(&out_srt, &segments, start)?;
    let chars: usize = segments
        .iter()
        .map(|s| s.display_text().chars().count())
        .sum();
    println!(
        "[转写] {:.2}s | 片段 {} | 字符 {} | 墙钟合计 {:.2}s",
        asr_sec,
        segments.len(),
        chars,
        extract_sec + prep_sec + asr_sec
    );
    println!("[输出] {:?}", out_srt);

    let _ = std::fs::remove_file(&clip);
    if asr_input != clip {
        let _ = std::fs::remove_file(&asr_input);
    }
    Ok(())
}
