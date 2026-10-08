//! SenseVoice 单进程 vs 多进程切块并行的实测对比。
//!
//! 默认忽略（需要本机 239MB 模型与样片）。运行：
//! `cargo test --offline --test bench_sensevoice_parallel -- --ignored --nocapture`

use std::path::PathBuf;
use std::time::Instant;
use voice2word::engines::{transcribe_chunked_sensevoice, FFmpegEngine, SenseVoiceEngine};
use voice2word::utils::AppConfig;

mod common;

#[test]
#[ignore]
fn compare_single_process_vs_chunked_parallel() {
    let test_name = "compare_single_process_vs_chunked_parallel";
    let config = AppConfig::load_from_file("config.toml").expect("加载 config.toml 失败");
    let ffmpeg_path = AppConfig::resolve_path(&config.paths.ffmpeg);
    let runner = AppConfig::resolve_path("tools/sensevoice_runner.py");
    let model = AppConfig::resolve_path("models/sensevoice/model.int8.onnx");
    let tokens = AppConfig::resolve_path("models/sensevoice/tokens.txt");
    let vad = AppConfig::resolve_path("models/sensevoice/silero_vad.onnx");
    let src = PathBuf::from("testVideo/03.1.3概率不等式.mp4");

    // 与其它集成测试同一套判据：CI 或本机缺任一资产 → 打印 SKIP 并尽早 return。
    // （本文件本就 `#[ignore]`，这里的 SKIP 只服务于显式 `--ignored` 的场景。）
    if common::skip_heavy(
        test_name,
        &[
            ("FFmpeg", &ffmpeg_path),
            ("tools/sensevoice_runner.py", &runner),
            ("models/sensevoice/model.int8.onnx", &model),
            ("models/sensevoice/tokens.txt", &tokens),
            ("models/sensevoice/silero_vad.onnx", &vad),
            ("样片 03.1.3概率不等式.mp4", &src),
        ],
    ) {
        return;
    }

    // 默认取 05:00 起 6 分钟；可用 V2W_BENCH_START / V2W_BENCH_SECONDS 调整
    let start_sec = std::env::var("V2W_BENCH_START").unwrap_or_else(|_| "300".into());
    let bench_sec: f64 = std::env::var("V2W_BENCH_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(360.0);
    let wav = std::env::temp_dir().join(format!("v2w_bench_{}s.wav", bench_sec as u32));
    if !wav.exists() {
        let ok = std::process::Command::new(&ffmpeg_path)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-ss",
                &start_sec,
                "-t",
            ])
            .arg(format!("{bench_sec}"))
            .arg("-i")
            .arg(&src)
            .args(["-vn", "-acodec", "pcm_s16le", "-ar", "16000", "-ac", "1"])
            .arg(&wav)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "抽取测试音频失败");
    }

    let engine = SenseVoiceEngine::new(runner, model, tokens, vad, 16);
    if !engine.is_available() {
        common::report_skip(test_name, "SenseVoice 模型或脚本未就绪");
        return;
    }

    let ffmpeg = FFmpegEngine::new(&ffmpeg_path);

    let started = Instant::now();
    let (single_segs, _) = engine
        .transcribe(&wav, Some("zh"), Some(8), Some(bench_sec), None)
        .expect("单进程转写失败");
    let single_sec = started.elapsed().as_secs_f64();

    // 可选：V2W_BENCH_WORKERS 显式指定并行进程数（对应设置页滑条），不设则走自动
    let workers_override: Option<usize> = std::env::var("V2W_BENCH_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok());

    let started = Instant::now();
    let (par_segs, _) = transcribe_chunked_sensevoice(
        &ffmpeg,
        &engine,
        &wav,
        bench_sec,
        Some("zh"),
        16,
        workers_override,
        None,
    )
    .expect("切块并行转写失败");
    let par_sec = started.elapsed().as_secs_f64();

    println!(
        "单进程 (8 线程)   : {single_sec:7.2}s  {} 句",
        single_segs.len()
    );
    println!("多进程切块并行    : {par_sec:7.2}s  {} 句", par_segs.len());
    println!("加速比            : {:.2}x", single_sec / par_sec);

    assert!(!par_segs.is_empty(), "切块并行必须产出字幕");
    assert!(
        par_segs.len() * 10 >= single_segs.len() * 6,
        "切块并行句数不应显著少于单进程: {} vs {}",
        par_segs.len(),
        single_segs.len()
    );
    assert!(
        par_sec < single_sec * 0.95,
        "切块并行应明显快于单进程: {par_sec:.2}s vs {single_sec:.2}s"
    );
}
