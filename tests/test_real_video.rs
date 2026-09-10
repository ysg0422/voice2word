use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use voice2word::core::TaskPipeline;
use voice2word::engines::{FFmpegEngine, LLMEngine, WhisperEngine};
use voice2word::utils::AppConfig;

#[tokio::test]
async fn test_real_video_pipeline() {
    let config = AppConfig::load_from_file("config.toml").expect("加载 config.toml 失败");
    println!("=== 开始真实视频全链路转写测试 ===");
    println!("FFmpeg 路径: {:?}", AppConfig::resolve_path(&config.paths.ffmpeg));
    println!("Whisper 路径: {:?}", AppConfig::resolve_path(&config.paths.whisper_cli));
    println!("Whisper 模型: {:?}", AppConfig::resolve_path(&config.paths.whisper_model));
    println!("LLM 路径: {:?}", AppConfig::resolve_path(&config.paths.llama_cli));
    println!("LLM 模型: {:?}", AppConfig::resolve_path(&config.paths.llm_model));

    assert!(AppConfig::resolve_path(&config.paths.whisper_model).exists(), "Whisper 模型必须真实存在！");
    assert!(AppConfig::resolve_path(&config.paths.llm_model).exists(), "LLM 模型必须真实存在！");

    let ffmpeg = Arc::new(FFmpegEngine::new(AppConfig::resolve_path(&config.paths.ffmpeg)));
    let whisper = Arc::new(WhisperEngine::new(
        AppConfig::resolve_path(&config.paths.whisper_cli),
        AppConfig::resolve_path(&config.paths.whisper_model),
        config.pipeline.whisper_threads,
        config.pipeline.whisper_processors,
    ));
    let llm = Arc::new(LLMEngine::new(
        AppConfig::resolve_path(&config.paths.llama_cli),
        AppConfig::resolve_path(&config.paths.llm_model),
        config.pipeline.llm_ctx,
        config.pipeline.llm_threads,
    ));

    let pipeline = Arc::new(TaskPipeline::new(ffmpeg, whisper, llm));
    let video_path = PathBuf::from("testVideo/03.1.3概率不等式.mp4");
    assert!(video_path.exists(), "测试视频必须存在！");

    let srt_out = PathBuf::from("testVideo/03.1.3概率不等式_success.srt");
    let (tx, mut rx) = mpsc::unbounded_channel();

    // 打印事件监听
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            println!("[流水线事件] {:?}", event);
        }
    });

    let start = std::time::Instant::now();
    let segments = pipeline
        .run(
            video_path,
            Some(srt_out.clone()),
            Some("zh".to_string()),
            "srt".to_string(),
            true, // 开启 0.5B 标点纠错润色
            Some(12), // 充分释放 16 核心 CPU 算力（12 线程并发）
            None,
            tx,
        )
        .await
        .expect("视频转写流水线执行失败");

    let elapsed = start.elapsed();
    println!("=============================================");
    println!("🎉 真实视频测试大获成功！");
    println!("总耗时: {:.1} 秒", elapsed.as_secs_f64());
    println!("成功生成字幕段数: {}", segments.len());
    if let Some(first) = segments.first() {
        println!("第一条字幕 [{}s -> {}s]: {}", first.start, first.end, first.text);
    }
    if let Some(last) = segments.last() {
        println!("最后一条字幕 [{}s -> {}s]: {}", last.start, last.end, last.text);
    }
    println!("SRT 输出文件大小: {} bytes", std::fs::metadata(&srt_out).map(|m| m.len()).unwrap_or(0));
    println!("=============================================");

    assert!(!segments.is_empty(), "必须成功生成字幕段！");
}
